//! Work placement (B4): which node runs a delegate child.
//!
//! The plan's rule (PLAN-compute-pool.md, "Placement"): sessions are
//! WHOLE-SESSION STICKY — a session lives where it was created
//! (`sessions.node`, B1.5, stamps it) — and the one scheduling decision
//! left is per-CHILD: `delegate_task backend = <node> | gpu | any`.
//!
//! Three spellings, one resolver:
//!
//! - `<id-or-name-or-prefix>` — THAT node, or an honest error. Never a
//!   silent fallback: asking for a node is a placement constraint
//!   (egress IP, GPU, filesystem locality), and quietly running the
//!   work somewhere else is the failure the constraint existed to
//!   prevent.
//! - `gpu` — any live node advertising a GPU, least-loaded first.
//!   "Any", not "the": with one GPU node today the two coincide, but
//!   the spelling describes the CONSTRAINT, not a machine.
//! - `any` — least-loaded across the pool INCLUDING this node. A pool
//!   of one is the degenerate but legal case: the local node wins by
//!   being the only candidate. Nodes whose load is unknown are ranked
//!   LAST but eligible (unknown is not idle, and not busy either); a
//!   STALE heartbeat makes a node ineligible — placing work on a node
//!   we have not heard from is not placement, it is hope.
//!
//! B4 deliberately does not add a scheduling layer for ordinary turns:
//! a turn runs where its session lives, and a session lives where it
//! was created. Sticky placement was decided 2026-09-20; what B4 adds
//! is the delegate knob the plan named, on the sticky foundation.
//!
//! ## What "least loaded" weighs (owner ask 306787.b)
//!
//! "Divide work across the available compute so that we don't
//! bottleneck so much on cpu and disk io." A node is ranked by
//! [`NodeLoad`]: its own 1-minute load per core (`load1`, 0-100), plus
//! one core's worth (`100 / cores` points) for every delegate child
//! counted against it, plus its disk IO pressure (PSI `some` avg10).
//! The children counted are the ones whose process runs there and the
//! ones placed there but not started (the node's own heartbeat says
//! both, and by which origin node), plus the ones THIS node placed there
//! that the node has not reported yet — never this node's twice. The
//! `workload` hint shifts the weights: `cpu` halves the IO term, `io`
//! doubles it and ranks every node over [`IO_HIGH`] after every node
//! under it. Unknown is never idle: an unknown load ranks after every
//! known one, unknown cores count a child as a quarter of the machine
//! (the old fixed 25), unknown IO adds nothing but ranks after a
//! measured low IO for `io` work.
//!
//! Splitting one large job into parts is NOT done here: the model
//! decides the fan-out (the `delegate_task` and `nodes` descriptions say
//! when to); this module only places each part.

use crate::store::{NodeMatch, NodeRow};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a `backend = …` argument asked for, parsed off the tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendSpec {
    /// `local` — explicitly this node (distinct from None, which never
    /// mentioned placement at all).
    Local,
    /// A node id, name, or unique prefix — resolved against the
    /// registry at call time, never cached.
    Node(String),
    /// Any live GPU node, least-loaded first.
    Gpu,
    /// Least-loaded live node including this one.
    Any,
}

impl BackendSpec {
    /// Parse the tool argument. Empty string = not a placement ask
    /// (treated as absent by the caller); unknown words are ERRORS,
    /// not defaults — a typo'd `vps2` must not quietly mean "any".
    pub fn parse(s: &str) -> Result<Option<BackendSpec>> {
        let t = s.trim();
        if t.is_empty() {
            return Ok(None);
        }
        Ok(Some(match t.to_ascii_lowercase().as_str() {
            "local" => BackendSpec::Local,
            "gpu" => BackendSpec::Gpu,
            "any" => BackendSpec::Any,
            _ => BackendSpec::Node(t.to_string()),
        }))
    }
}

/// One resolved placement decision — the node that will run the child,
/// with enough context for the tool to say WHAT it did.
#[derive(Debug, Clone)]
pub struct Placement {
    pub node: NodeRow,
    /// Why this node: echoed in the tool result so the caller (model
    /// and, through it, the operator) can audit the decision.
    pub reason: String,
}

/// A node's GPU name, parsed out of the registry's raw JSON column.
fn gpu_name(row: &NodeRow) -> Option<String> {
    let raw = row.gpu.as_deref()?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get("name")?.as_str().map(|s| s.to_string())
}

/// Resolve a spec against the live registry.
///
/// `stale_after` and `me` come from the caller (config + node.toml)
/// rather than being re-read here: this function is pure over its
/// inputs so tests can pin every branch without env games.
///
/// `routable` says whether a node can be DIALED from here (this node
/// always can; a remote one needs an `[a2a_agents]` entry). `gpu` and
/// `any` choose only among routable nodes: they are preferences, and
/// picking a node the work cannot reach turned a free pool into an
/// error. A NAMED node is a constraint, so it is resolved regardless
/// and an unreachable one stays an honest error at dial time.
pub fn resolve(
    store: &crate::store::Store,
    spec: &BackendSpec,
    me: Option<&str>,
    stale_after: i64,
    routable: &dyn Fn(&NodeRow) -> bool,
) -> Result<Placement> {
    resolve_needing(store, spec, me, stale_after, routable, &[])
}

/// The programs of `needs` that `node` does not advertise
/// (`NodeRow::caps_tools`); all of them for a node that advertises
/// nothing, since an unknown is not a yes.
fn missing<'a>(node: &NodeRow, needs: &'a [String]) -> Vec<&'a str> {
    let has = node.caps_tools().unwrap_or_default();
    needs
        .iter()
        .map(String::as_str)
        .filter(|n| !has.iter().any(|h| h == n))
        .collect()
}

/// [`resolve`] with `needs` (PLAN-phone-userland.md T2, `delegate_task
/// needs`): programs the node must ALL advertise. `gpu` and `any` choose
/// only among nodes that do; a named node (or `local`) that lacks one is
/// an error naming what it lacks, never a quiet move elsewhere.
pub fn resolve_needing(
    store: &crate::store::Store,
    spec: &BackendSpec,
    me: Option<&str>,
    stale_after: i64,
    routable: &dyn Fn(&NodeRow) -> bool,
    needs: &[String],
) -> Result<Placement> {
    resolve_workload(
        store,
        spec,
        me,
        stale_after,
        routable,
        needs,
        Workload::General,
    )
}

/// [`resolve_needing`] with the call's `workload` hint (306787.b): what
/// the child will mostly spend — CPU, disk IO, or a mix (the default).
/// It changes only how `gpu`/`any` rank their candidates
/// ([`rank_key`]); a named node is a constraint and is never moved, but
/// its reason says when IO work lands on a node with high IO pressure.
pub fn resolve_workload(
    store: &crate::store::Store,
    spec: &BackendSpec,
    me: Option<&str>,
    stale_after: i64,
    routable: &dyn Fn(&NodeRow) -> bool,
    needs: &[String],
    workload: Workload,
) -> Result<Placement> {
    // Rows whose process has exited are not work. Pruned here as well as
    // on the heartbeat tick, so a placement right after a crash does not
    // count the dead.
    if matches!(spec, BackendSpec::Any | BackendSpec::Gpu) {
        if let Err(e) = prune(store.conn(), &pid_lives) {
            tracing::debug!("placement: prune failed: {e:#}");
        }
    }
    let lacks = |what: &str, node: &NodeRow| -> Result<()> {
        let gone = missing(node, needs);
        if gone.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "{what} ({}) does not have {} — needs = [{}]; `veles nodes` lists what each node has",
            node.name,
            gone.join(", "),
            needs.join(", ")
        )
    };
    match spec {
        BackendSpec::Local => match me {
            Some(id) => match store.node_get(id)? {
                Some(node) => {
                    lacks("backend=local: this node", &node)?;
                    Ok(Placement {
                        reason: "explicitly local".into(),
                        node,
                    })
                }
                None => anyhow::bail!(
                    "backend=local: this node ({id}) is not in the registry — \
                     run `veles nodes probe` or start a gateway first"
                ),
            },
            None => anyhow::bail!(
                "backend=local: no node identity on this machine \
                 (no node.toml) — placement needs to know who 'local' is"
            ),
        },
        BackendSpec::Node(want) => {
            // Names are what operators actually type (`backend = "vps"`
            // beats `backend = "9f3a…"`); ids and unique id prefixes
            // remain the unambiguous spelling. Exact name match first,
            // then the id resolution — a name that happens to equal
            // another node's id prefix resolves the ID (ids are the
            // identity; names are display).
            let full = match store.node_resolve(want)? {
                NodeMatch::One(row) => *row,
                NodeMatch::None => {
                    let by_name = store.nodes_list()?.into_iter().find(|n| n.name == *want);
                    match by_name {
                        Some(row) => row,
                        None => anyhow::bail!(
                            "backend node '{want}': no such node — /nodes lists the registry"
                        ),
                    }
                }
                NodeMatch::Ambiguous(n) => {
                    anyhow::bail!("backend node '{want}': ambiguous prefix ({n} nodes match)")
                }
            };
            if crate::node::is_stale(&full.heartbeat_at, stale_after) {
                anyhow::bail!(
                    "backend node '{want}' ({}) is STALE — no heartbeat within {stale_after}s; \
                     refusing to place work on a node we have not heard from",
                    full.name
                );
            }
            lacks(&format!("backend node '{want}'"), &full)?;
            // A constraint is never moved, but IO work sent to a node
            // whose disk is already stalling should say so.
            let warn = match full.io_some {
                Some(io) if workload == Workload::Io && io >= IO_HIGH => {
                    format!(" (note: its IO pressure is {io}%, high for workload='io')")
                }
                _ => String::new(),
            };
            Ok(Placement {
                reason: format!("named: {want}{warn}"),
                node: full,
            })
        }
        BackendSpec::Gpu => {
            let live: Vec<NodeRow> = store
                .nodes_list()?
                .into_iter()
                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))
                .collect();
            let gpu_live: Vec<NodeRow> = live
                .iter()
                .filter(|n| gpu_name(n).is_some())
                .filter(|n| missing(n, needs).is_empty())
                .cloned()
                .collect();
            if gpu_live.is_empty() {
                let total = store.nodes_list()?.len();
                if !needs.is_empty() {
                    anyhow::bail!(
                        "backend=gpu: no live GPU node has all of needs = [{}] \
                         ({total} node(s) in the registry; stale ones excluded)",
                        needs.join(", ")
                    );
                }
                anyhow::bail!(
                    "backend=gpu: no live node advertises a GPU \
                     ({total} node(s) in the registry; stale ones excluded)"
                );
            }
            let gpus: Vec<NodeRow> = gpu_live.iter().filter(|n| routable(n)).cloned().collect();
            if gpus.is_empty() {
                anyhow::bail!(
                    "backend=gpu: {} live GPU node(s), none reachable from here — \
                     a remote node needs an [a2a_agents] entry with its url and token",
                    gpu_live.len()
                );
            }
            let ranked = rank(store.conn(), me, gpus, workload);
            let (node, load) = ranked.into_iter().next().expect("nonempty");
            Ok(Placement {
                reason: format!(
                    "gpu: {} ({}) — {}",
                    gpu_name(&node).unwrap_or_default(),
                    node.name,
                    load.describe(workload)
                ),
                node,
            })
        }
        BackendSpec::Any => {
            let all = store.nodes_list()?;
            if all.is_empty() {
                anyhow::bail!("backend=any: the registry is empty — nothing to place on");
            }
            let live: Vec<NodeRow> = all
                .into_iter()
                .filter(|n| !crate::node::is_stale(&n.heartbeat_at, stale_after))
                .collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: every registry node is stale \
                     (no heartbeat within {stale_after}s)"
                );
            }
            let n_live = live.len();
            let live: Vec<NodeRow> = live
                .into_iter()
                .filter(|n| missing(n, needs).is_empty())
                .collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: none of {n_live} live node(s) has all of needs = [{}] — \
                     `veles nodes` lists what each node has",
                    needs.join(", ")
                );
            }
            let live: Vec<NodeRow> = live.into_iter().filter(|n| routable(n)).collect();
            if live.is_empty() {
                anyhow::bail!(
                    "backend=any: {n_live} live node(s), none reachable from here — \
                     a remote node needs an [a2a_agents] entry with its url and token"
                );
            }
            // Least-loaded first; unknown load ranks last but stays
            // eligible — see the module doc for why unknown ≠ idle.
            let ranked = rank(store.conn(), me, live, workload);
            let (node, load) = ranked.into_iter().next().expect("nonempty");
            Ok(Placement {
                reason: format!("least-loaded: {}", load.describe(workload)),
                node,
            })
        }
    }
}

// ── the workload hint ────────────────────────────────────────────────

/// What a child will mostly spend (`delegate_task workload`). The model
/// knows its task; placement only knows the nodes — so the hint is the
/// model's, and an unknown word is an error, not a default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Workload {
    /// A mix, or not said: CPU load and IO pressure weigh the same.
    #[default]
    General,
    /// Compiling, encoding, compressing, test suites: CPU first, IO at
    /// half weight (a build still writes).
    Cpu,
    /// Large copies, scans, archive or database dumps, downloads to
    /// disk: IO pressure at double weight, and a node over [`IO_HIGH`]
    /// is chosen only when every candidate is.
    Io,
}

impl Workload {
    pub fn parse(s: &str) -> Result<Workload> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "" | "general" | "mixed" => Workload::General,
            "cpu" => Workload::Cpu,
            "io" | "disk" => Workload::Io,
            other => anyhow::bail!(
                "workload '{other}': say 'cpu' (compile, encode, test), 'io' (copy, scan, \
                 dump, download) or leave it out for mixed work"
            ),
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Workload::General => "general",
            Workload::Cpu => "cpu",
            Workload::Io => "io",
        }
    }
}

/// IO pressure (PSI `some` avg10, percent) at and above which a node is
/// avoided for `workload = io`. A fifth of wall time with a task stalled
/// on the disk is a disk that a further copy or scan queues behind; an
/// idle machine sits at 0-2.
pub const IO_HIGH: i64 = 20;

/// The core count assumed for a node that does not report one (a 0.1.6
/// peer): one child is then a quarter of the machine, the fixed `+25`
/// this weight replaced.
pub const DEFAULT_CORES: i64 = 4;

// ── what one node is ranked by ───────────────────────────────────────

/// Everything placement counts on one node, from its registry row and
/// this node's `placement_work` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeLoad {
    /// The node's reported CPU count (`None`: a 0.1.6 node).
    pub cores: Option<i64>,
    /// Its own 1-minute load per core, 0-100 (`None`: unknown).
    pub load_percent: Option<i64>,
    pub io_some: Option<i64>,
    pub io_full: Option<i64>,
    /// Delegate children whose process runs there. For this node, live
    /// from the work table; for a peer, its last heartbeat (`None`: it
    /// does not report work — a 0.1.6 node).
    pub running: Option<i64>,
    /// Children placed there and not started yet (same sources).
    pub queued: Option<i64>,
    /// Children THIS node placed there that its report does not include
    /// yet (all of them for a node that does not report). Always 0 for
    /// this node, whose own count is live.
    pub unreported_from_here: i64,
}

impl NodeLoad {
    /// Every child counted against the node.
    pub fn children(&self) -> i64 {
        self.running.unwrap_or(0) + self.queued.unwrap_or(0) + self.unreported_from_here
    }

    /// What one more child adds, in hundredths of a load-per-core
    /// percent: one core's worth.
    pub fn child_weight_x100(&self) -> i64 {
        10_000 / self.cores.unwrap_or(DEFAULT_CORES).max(1)
    }

    /// The node's load per core with its counted children added, in
    /// hundredths of a percent (`None` while its own load is unknown).
    pub fn projected_x100(&self) -> Option<i64> {
        self.load_percent
            .map(|l| l * 100 + self.children() * self.child_weight_x100())
    }

    pub fn io_high(&self) -> bool {
        self.io_some.is_some_and(|io| io >= IO_HIGH)
    }

    /// One line for a placement reason: what was weighed, and the
    /// figure it came to.
    pub fn describe(&self, w: Workload) -> String {
        let cores = match self.cores {
            Some(c) => format!("{c} cores"),
            None => format!("cores unknown (counted as {DEFAULT_CORES})"),
        };
        let load = match self.load_percent {
            Some(l) => format!("load {l}%/core"),
            None => "load unknown".to_string(),
        };
        let io = match self.io_some {
            Some(io) if io >= IO_HIGH => format!("io {io}% (high)"),
            Some(io) => format!("io {io}%"),
            None => "io unknown".to_string(),
        };
        let projected = self
            .projected_x100()
            .map(|p| format!(" → {}%/core projected", p / 100))
            .unwrap_or_default();
        format!(
            "{load} on {cores}, {io}, {} child(ren) counted{projected}, workload {}",
            self.children(),
            w.as_str()
        )
    }
}

/// [`NodeLoad`] for `n`, as this node (`me`) sees it. `conn` is this
/// node's store (the `placement_work` table).
pub fn node_load(conn: &rusqlite::Connection, me: Option<&str>, n: &NodeRow) -> NodeLoad {
    // A peer's figures arrive over the wire: a percentage outside 0-100,
    // a core count under 1 or a negative child count is a broken report,
    // not a reason to rank the node first. Clamped, or unknown.
    let percent = |v: Option<i64>| v.map(|v| v.clamp(0, 100));
    let count = |v: Option<i64>| v.map(|v| v.max(0));
    let mut l = NodeLoad {
        cores: n.cores.filter(|&c| c > 0),
        load_percent: percent(n.load1),
        io_some: percent(n.io_some),
        io_full: percent(n.io_full),
        running: count(n.children),
        queued: count(n.queued),
        unreported_from_here: 0,
    };
    if me == Some(n.id.as_str()) {
        // Our own work is live, not a heartbeat old.
        if let Ok(own) = own_report(conn, &n.id) {
            l.running = Some(i64::from(own.running));
            l.queued = Some(i64::from(own.queued));
        }
        return l;
    }
    let placed = placed_on(conn, &n.id);
    l.unreported_from_here = match (l.running, me) {
        // The peer reports its work, by origin: what it already counts
        // from us is not added again.
        (Some(_), Some(me)) => {
            let counted = WorkReport::parse_from(n.children_from.as_deref())
                .get(me)
                .copied()
                .unwrap_or(0);
            (placed - counted).max(0)
        }
        _ => placed,
    };
    l
}

/// The order `gpu`/`any` (and the automatic tool homes) try nodes in,
/// as a sortable key: avoided-for-IO tier, unknown load, weighted score.
/// Ties then break by `sessions_served` (spread work) and id
/// (determinism — equal candidates must not flip between calls, or
/// repeated placements would oscillate); see [`rank`].
pub fn rank_key(l: &NodeLoad, w: Workload) -> (u8, u8, i64) {
    let unknown = u8::from(l.load_percent.is_none());
    let cpu = l
        .projected_x100()
        .unwrap_or(l.children() * l.child_weight_x100());
    let io = l.io_some.unwrap_or(0) * 100;
    let score = match w {
        Workload::General => cpu + io,
        Workload::Cpu => 2 * cpu + io,
        Workload::Io => cpu + 2 * io,
    };
    let tier = match (w, l.io_some) {
        (Workload::Io, Some(io)) if io >= IO_HIGH => 2,
        (Workload::Io, None) => 1,
        _ => 0,
    };
    (tier, unknown, score)
}

/// `nodes`, least loaded first for `w`, each with what it was ranked by.
pub fn rank(
    conn: &rusqlite::Connection,
    me: Option<&str>,
    nodes: Vec<NodeRow>,
    w: Workload,
) -> Vec<(NodeRow, NodeLoad)> {
    let mut v: Vec<(NodeRow, NodeLoad)> = nodes
        .into_iter()
        .map(|n| {
            let l = node_load(conn, me, &n);
            (n, l)
        })
        .collect();
    v.sort_by(|(a, la), (b, lb)| {
        rank_key(la, w)
            .cmp(&rank_key(lb, w))
            .then_with(|| a.sessions_served.cmp(&b.sessions_served))
            .then_with(|| a.id.cmp(&b.id))
    });
    v
}

/// The registry's placement figures for one `nodes list` line, after
/// `load=`: `  cores=16 io=3% children=2+1` (`?` where the node does not
/// report one — an older build, or no IO counter on its platform).
pub fn row_metrics(n: &NodeRow) -> String {
    let shown = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let io = match n.io_some {
        Some(io) => format!("{io}%"),
        None => "?".into(),
    };
    let children = match (n.children, n.queued) {
        (Some(r), Some(q)) if q > 0 => format!("{r}+{q}"),
        (r, _) => shown(r),
    };
    format!("  cores={} io={io} children={children}", shown(n.cores))
}

// ── the work table: what is placed and what runs ─────────────────────
//
// SI-7 counted each child placed from here in a process-local map, so a
// batch spread instead of piling onto one heartbeat's idlest node. That
// map saw neither another process on this node (a TUI's delegates) nor
// another node's placements, and a gateway restart forgot children that
// were still running. The counts now live in `placement_work`
// (state.db, local, never gossiped), each row bounded by a process, and
// the heartbeat publishes this node's own counts by origin.

/// What a node says about its delegate work in its heartbeat.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkReport {
    /// Children whose process runs on the node now.
    pub running: u32,
    /// Children placed on it by its own sessions and not started yet.
    pub queued: u32,
    /// `running` by the node whose session asked for each.
    pub from: BTreeMap<String, u32>,
}

impl WorkReport {
    /// The `children_from` column: `{origin: running}` as JSON.
    pub fn from_json(&self) -> String {
        serde_json::to_string(&self.from).unwrap_or_else(|_| "{}".into())
    }

    /// The `children_from` column read back; empty when absent or
    /// unreadable.
    pub fn parse_from(raw: Option<&str>) -> BTreeMap<String, i64> {
        raw.and_then(|r| serde_json::from_str(r).ok())
            .unwrap_or_default()
    }
}

/// Is the process that bounds a work row still the one that wrote it?
/// Alive, and — where the platform has a lifetime identity — the same
/// lifetime (a recycled PID is somebody else).
pub fn pid_lives(pid: u32, start: Option<&str>) -> bool {
    crate::platform::pid_alive(pid)
        && match start {
            Some(s) => crate::platform::process_start_id(pid).as_deref() == Some(s),
            None => true,
        }
}

/// What [`prune`] found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pruned {
    pub kept: usize,
    pub dropped: usize,
}

/// Drop every work row whose process is gone (`alive` is
/// [`pid_lives`] outside tests). Run at gateway boot (the
/// reconciliation with what is actually running), on every heartbeat
/// tick, and before a `gpu`/`any` resolution.
pub fn prune(
    conn: &rusqlite::Connection,
    alive: &dyn Fn(u32, Option<&str>) -> bool,
) -> Result<Pruned> {
    let rows: Vec<(i64, i64, Option<String>)> = {
        let mut st = conn.prepare("SELECT id, pid, pid_start FROM placement_work")?;
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    let mut out = Pruned::default();
    for (id, pid, start) in rows {
        let lives = u32::try_from(pid).is_ok_and(|p| alive(p, start.as_deref()));
        if lives {
            out.kept += 1;
        } else {
            conn.execute(
                "DELETE FROM placement_work WHERE id = ?1",
                rusqlite::params![id],
            )?;
            out.dropped += 1;
        }
    }
    Ok(out)
}

/// This node's own [`WorkReport`]: running rows targeting it, by origin,
/// and its sessions' placements on it that have not started.
pub fn own_report(conn: &rusqlite::Connection, me: &str) -> Result<WorkReport> {
    let mut r = WorkReport::default();
    {
        let mut st = conn.prepare(
            "SELECT origin, COUNT(*) FROM placement_work
             WHERE state = 'running' AND target = ?1 GROUP BY origin",
        )?;
        let rows = st.query_map(rusqlite::params![me], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (origin, n) = row?;
            let n = u32::try_from(n).unwrap_or(u32::MAX);
            r.running = r.running.saturating_add(n);
            r.from.insert(origin, n);
        }
    }
    r.queued = conn
        .query_row(
            "SELECT COUNT(*) FROM placement_work WHERE state = 'placed' AND target = ?1",
            rusqlite::params![me],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| u32::try_from(n).unwrap_or(u32::MAX))?;
    Ok(r)
}

/// Children this node's processes placed on `target` that are held as
/// claims (a remote target's whole life; a local one's until it starts).
fn placed_on(conn: &rusqlite::Connection, target: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM placement_work WHERE state = 'placed' AND target = ?1",
        rusqlite::params![target],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Every work row targeting `node_id` in this node's store — claims and
/// running children alike. What the `nodes` tool and the tests read.
pub fn in_flight(node_id: &str) -> usize {
    work_db()
        .and_then(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM placement_work WHERE target = ?1",
                rusqlite::params![node_id],
                |r| r.get::<_, i64>(0),
            )
            .ok()
        })
        .map(|n| n.max(0) as usize)
        .unwrap_or(0)
}

fn default_db() -> PathBuf {
    crate::config::veles_home().join("state.db")
}

/// A short-lived connection to the work table: claims and child guards
/// are written and released from blocking threads, the async runtime
/// and drop glue, none of which holds the gateway's store handle. The
/// table itself is created by `Store::open` (every placement resolves
/// through one first).
fn open_work(db: &Path) -> Option<rusqlite::Connection> {
    let c = rusqlite::Connection::open(db).ok()?;
    c.busy_timeout(std::time::Duration::from_secs(5)).ok()?;
    Some(c)
}

fn work_db() -> Option<rusqlite::Connection> {
    open_work(&default_db())
}

fn this_node() -> Option<String> {
    crate::node::NodeIdentity::load(&crate::config::config_dir())
        .ok()
        .flatten()
        .map(|n| n.id)
}

/// One child's hold on its node from the moment it is placed until it
/// finishes (drops). A claim on THIS node becomes its own child's
/// running row when that child's process starts ([`child_started`] is
/// handed the claim); a claim on a peer is held for the whole remote
/// call.
#[must_use = "the claim counts the child only while it is held"]
pub struct Claim {
    db: PathBuf,
    row: Option<i64>,
}

impl Claim {
    /// The work row this claim holds (`None`: it could not be recorded).
    pub fn row(&self) -> Option<i64> {
        self.row
    }
}

/// Whose delegate child a process is, for the work count
/// ([`child_started`]).
#[derive(Clone, Copy)]
pub enum ChildOf<'a> {
    /// One of this node's own sessions, with the claim its `any`/`gpu`
    /// placement holds (`None`: it was never placed, so nothing was
    /// claimed for it).
    Here(Option<&'a Claim>),
    /// A remote task, for the placing node with this id.
    Peer(&'a str),
}

impl Drop for Claim {
    fn drop(&mut self) {
        let Some(row) = self.row else { return };
        if let Some(c) = open_work(&self.db) {
            // Only a claim that is still a claim: a row its child took
            // over is the child's, and its guard deletes it.
            let _ = c.execute(
                "DELETE FROM placement_work WHERE id = ?1 AND state = 'placed'",
                rusqlite::params![row],
            );
        }
    }
}

/// Count a child just placed on `node_id` until the returned claim drops.
pub fn claim(node_id: &str) -> Claim {
    claim_in(&default_db(), node_id, this_node().as_deref().unwrap_or(""))
}

/// [`claim`] against an explicit store and origin (tests, and any caller
/// that already holds them).
pub fn claim_in(db: &Path, node_id: &str, origin: &str) -> Claim {
    let pid = std::process::id();
    let row = open_work(db).and_then(|c| {
        c.execute(
            "INSERT INTO placement_work (state, target, origin, pid, pid_start)
             VALUES ('placed', ?1, ?2, ?3, ?4)",
            rusqlite::params![
                node_id,
                origin,
                i64::from(pid),
                crate::platform::process_start_id(pid)
            ],
        )
        .map_err(|e| tracing::warn!("placement: claim on {node_id} not recorded: {e}"))
        .ok()
        .map(|_| c.last_insert_rowid())
    });
    Claim {
        db: db.to_path_buf(),
        row,
    }
}

/// A delegate child's process, counted on this node while it runs.
#[must_use = "the child is counted only while the guard is held"]
pub struct ChildRun {
    db: PathBuf,
    row: i64,
}

impl Drop for ChildRun {
    fn drop(&mut self) {
        if let Some(c) = open_work(&self.db) {
            let _ = c.execute(
                "DELETE FROM placement_work WHERE id = ?1",
                rusqlite::params![self.row],
            );
        }
    }
}

/// A child process just started on this node. Its row is bounded by the
/// CHILD's pid, so it outlives a gateway restart exactly as long as the
/// child does. A child placed here by `any`/`gpu` takes over ITS OWN
/// claim's row, so it is counted once. Only its own: when claims were
/// interchangeable, a child could take another member's claim, and if
/// that member then failed before its child started, the leftover claim
/// was counted as queued work until the other member finished. `None`
/// without a node identity, since no registry row would carry the count.
pub fn child_started(of: ChildOf<'_>, child_pid: u32) -> Option<ChildRun> {
    let me = this_node()?;
    let db = match of {
        ChildOf::Here(Some(c)) => c.db.clone(),
        _ => default_db(),
    };
    child_started_in(&db, &me, of, child_pid)
}

/// [`child_started`] against an explicit store and identity.
pub fn child_started_in(db: &Path, me: &str, of: ChildOf<'_>, child_pid: u32) -> Option<ChildRun> {
    let c = open_work(db)?;
    let start = crate::platform::process_start_id(child_pid);
    let (origin, claim_row) = match of {
        ChildOf::Here(claim) => (me, claim.and_then(Claim::row)),
        ChildOf::Peer(origin) => (origin, None),
    };
    // The claim's row, if it is still a claim on this node; a claim that
    // could not be recorded, or whose row is gone, gets a row of its own.
    let took = match claim_row {
        Some(id) => c
            .execute(
                "UPDATE placement_work SET state = 'running', pid = ?2, pid_start = ?3
                 WHERE id = ?1 AND state = 'placed' AND target = ?4",
                rusqlite::params![id, i64::from(child_pid), start, me],
            )
            .ok()
            .filter(|&n| n == 1)
            .map(|_| id),
        None => None,
    };
    let row = match took {
        Some(id) => id,
        None => {
            c.execute(
                "INSERT INTO placement_work (state, target, origin, pid, pid_start)
                 VALUES ('running', ?1, ?2, ?3, ?4)",
                rusqlite::params![me, origin, i64::from(child_pid), start],
            )
            .ok()?;
            c.last_insert_rowid()
        }
    };
    Some(ChildRun {
        db: db.to_path_buf(),
        row,
    })
}

/// Does this placement land on THIS node? The caller runs the child
/// locally when it does (sticky: local execution is the degenerate
/// case of remote, not a separate code path with its own bugs).
pub fn is_local(p: &Placement, me: Option<&str>) -> bool {
    me == Some(p.node.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> crate::store::Store {
        let dir = tempfile::tempdir().unwrap();
        crate::store::Store::open(&dir.path().join("state.db")).unwrap()
    }

    fn reg(store: &crate::store::Store, id: &str, name: &str, gpu: Option<&str>) {
        store
            .node_register(id, name, "h", "0", "always-on", gpu, None, None, None)
            .unwrap();
    }

    /// The parse: keywords, free-form names, empty = absent, and the
    /// case-insensitivity of the keywords only (node NAMES keep their
    /// case — they are identifiers, not flags).
    #[test]
    fn spec_parse_keywords_names_and_case() {
        assert_eq!(BackendSpec::parse("").unwrap(), None);
        assert_eq!(BackendSpec::parse("  ").unwrap(), None);
        assert_eq!(
            BackendSpec::parse("local").unwrap(),
            Some(BackendSpec::Local)
        );
        assert_eq!(BackendSpec::parse("GPU").unwrap(), Some(BackendSpec::Gpu));
        assert_eq!(BackendSpec::parse("Any").unwrap(), Some(BackendSpec::Any));
        assert_eq!(
            BackendSpec::parse("vps").unwrap(),
            Some(BackendSpec::Node("vps".into()))
        );
        // Keywords are ASCII-case-insensitive; node NAMES keep their
        // case. A node named exactly "local"/"gpu"/"any" is therefore
        // unreachable through this argument — the registry id or a
        // prefix still reaches it, and nobody has ever named a machine
        // that way.
        assert_eq!(
            BackendSpec::parse("LOCAL").unwrap(),
            Some(BackendSpec::Local)
        );
    }

    /// The named-node resolution: by id, by name, by prefix, and the
    /// honest errors for none/ambiguous/stale.
    #[test]
    fn named_node_resolves_and_refuses() {
        let store = scratch();
        reg(&store, "aaaa-1", "vps", None);
        reg(&store, "aabb-2", "laptop", None);
        // by id
        let p = resolve(
            &store,
            &BackendSpec::Node("aaaa-1".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap();
        assert_eq!(p.node.id, "aaaa-1");
        // by name
        let p = resolve(
            &store,
            &BackendSpec::Node("laptop".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap();
        assert_eq!(p.node.id, "aabb-2");
        // ambiguous prefix
        let err =
            resolve(&store, &BackendSpec::Node("a".into()), None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("ambiguous"), "{err:#}");
        // unknown name
        let err = resolve(
            &store,
            &BackendSpec::Node("nope".into()),
            None,
            600,
            &|_| true,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no such node"), "{err:#}");
        // named but STALE: refused too — naming a node does not make a
        // dead one a placement target ("the work will run there" is
        // the promise; a node we have not heard from cannot keep it).
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = '2001-01-01 00:00:00' WHERE id = 'aaaa-1'",
                [],
            )
            .unwrap();
        let err = resolve(&store, &BackendSpec::Node("vps".into()), None, 600, &|_| {
            true
        })
        .unwrap_err();
        assert!(err.to_string().contains("STALE"), "{err:#}");
    }

    /// `gpu` picks the GPU node, and says WHICH card; no live GPU node
    /// is an honest error, not a fallback to CPU.
    #[test]
    fn gpu_picks_a_gpu_node_or_refuses() {
        let store = scratch();
        reg(&store, "g1", "laptop", Some(r#"{"name":"RTX 3080"}"#));
        reg(&store, "c1", "vps", None);
        let p = resolve(&store, &BackendSpec::Gpu, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "g1");
        assert!(p.reason.contains("RTX 3080"), "{}", p.reason);

        let bare = scratch();
        reg(&bare, "c1", "vps", None);
        let err = resolve(&bare, &BackendSpec::Gpu, None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("no live node advertises a GPU"));
    }

    /// `any` is least-loaded INCLUDING this node, unknown ranks last,
    /// and a stale node is never placed on.
    #[test]
    fn any_is_least_loaded_with_unknown_last_and_stale_excluded() {
        let store = scratch();
        reg(&store, "busy", "a", None);
        reg(&store, "idle", "b", None);
        reg(&store, "quiet", "c", None);
        store.node_heartbeat("busy", Some(80)).unwrap();
        store.node_heartbeat("idle", Some(10)).unwrap();
        // quiet: no load row at all (unknown)
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "idle");
        assert!(p.reason.contains("least-loaded"));

        // unknown ranks after every known value — even a known 100
        // beats an unknown (both `busy` and `idle` sit at 100; the tie
        // breaks to `busy` by id).
        store.node_heartbeat("idle", Some(100)).unwrap();
        store.node_heartbeat("busy", Some(100)).unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "busy", "unknown load ranks last, not first");

        // stale = ineligible. Make everyone stale except `idle`.
        let old = "2001-01-01 00:00:00";
        store
            .conn()
            .execute(
                "UPDATE nodes SET heartbeat_at = ?1 WHERE id != 'idle'",
                rusqlite::params![old],
            )
            .unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "idle");
    }

    /// A single-node registry is a legal `any` (the degenerate pool),
    /// and `local` needs an identity plus a registry row.
    #[test]
    fn single_node_any_and_local() {
        let store = scratch();
        reg(&store, "me-1", "solo", None);
        let p = resolve(&store, &BackendSpec::Any, Some("me-1"), 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "me-1");
        assert!(is_local(&p, Some("me-1")));

        let err = resolve(&store, &BackendSpec::Local, None, 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("no node identity"));

        let err = resolve(&store, &BackendSpec::Local, Some("ghost"), 600, &|_| true).unwrap_err();
        assert!(err.to_string().contains("not in the registry"));
    }

    /// `any` and `gpu` are preferences: they choose among nodes the
    /// work can REACH. B4 ranked every live node, so an idle peer with
    /// no dialing route won `any` and the call failed while this node
    /// sat free. A named node is a constraint and still resolves.
    #[test]
    fn any_and_gpu_choose_only_reachable_nodes() {
        let store = scratch();
        reg(&store, "me-1", "laptop", None);
        reg(&store, "far-2", "vps", Some(r#"{"name":"A100"}"#));
        store.node_heartbeat("me-1", Some(90)).unwrap();
        store.node_heartbeat("far-2", Some(5)).unwrap();
        let only_me = |n: &NodeRow| n.id == "me-1";
        // The idle peer is unreachable: `any` lands here, not there.
        let p = resolve(&store, &BackendSpec::Any, Some("me-1"), 600, &only_me).unwrap();
        assert_eq!(p.node.id, "me-1");
        // The only GPU is unreachable: an honest error that says so.
        let err = resolve(&store, &BackendSpec::Gpu, Some("me-1"), 600, &only_me).unwrap_err();
        assert!(err.to_string().contains("none reachable"), "{err:#}");
        // Named: still resolved; the dial reports the missing route.
        let p = resolve(
            &store,
            &BackendSpec::Node("vps".into()),
            Some("me-1"),
            600,
            &only_me,
        )
        .unwrap();
        assert_eq!(p.node.id, "far-2");
    }

    /// The comparator's determinism: equal load + equal served breaks
    /// by id, so repeated resolutions cannot oscillate.
    #[test]
    fn load_order_is_deterministic_on_ties() {
        let store = scratch();
        reg(&store, "zz", "x", None);
        reg(&store, "aa", "y", None);
        store.node_heartbeat("zz", Some(42)).unwrap();
        store.node_heartbeat("aa", Some(42)).unwrap();
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "aa", "id breaks the tie, deterministically");
    }

    fn reg_with_tools(
        store: &crate::store::Store,
        id: &str,
        name: &str,
        gpu: Option<&str>,
        tools: &[&str],
        load: u8,
    ) {
        let caps = serde_json::json!({ "os": "linux", "tools": tools }).to_string();
        store
            .node_register(
                id,
                name,
                "h",
                "0",
                "always-on",
                gpu,
                None,
                None,
                Some(&caps),
            )
            .unwrap();
        store.node_heartbeat(id, Some(load)).unwrap();
    }

    /// A store whose file outlives the fixture, for the tests that open
    /// their own connections to it (claims and child guards do).
    fn scratch_at() -> (tempfile::TempDir, crate::store::Store, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let store = crate::store::Store::open(&path).unwrap();
        (dir, store, path)
    }

    fn work_rows(store: &crate::store::Store, target: &str) -> i64 {
        store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM placement_work WHERE target = ?1",
                rusqlite::params![target],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// Report cores, IO pressure and work for a registered node, the way
    /// its own heartbeat would.
    fn beat(
        store: &crate::store::Store,
        id: &str,
        load: u8,
        cores: Option<u32>,
        io: Option<u8>,
        work: Option<WorkReport>,
    ) {
        store
            .node_heartbeat_sample(
                id,
                &crate::store::HeartbeatSample {
                    load1: Some(load),
                    cores,
                    io: io.map(|s| crate::node::IoPressure {
                        some: Some(s),
                        full: Some(0),
                    }),
                    work,
                },
            )
            .unwrap();
    }

    /// SI-7: a fan-out spreads. Each child placed from here counts on
    /// its node until it finishes, so three `any` placements in a row
    /// do not pile onto the one node whose heartbeat looked idlest; a
    /// node that is truly far less loaded still takes the next one.
    /// (Nodes without a core count weigh a child as a quarter of the
    /// machine — the old fixed 25, now [`DEFAULT_CORES`].)
    #[test]
    fn any_counts_children_already_placed_until_they_finish() {
        let (_dir, store, db) = scratch_at();
        reg_with_tools(&store, "si7-laptop", "laptop", None, &[], 10);
        reg_with_tools(&store, "si7-vps", "vps", None, &[], 20);
        reg_with_tools(&store, "si7-phone", "phone", None, &[], 90);
        let pick = || {
            resolve(&store, &BackendSpec::Any, None, 600, &|n| {
                n.id.starts_with("si7-")
            })
            .unwrap()
        };
        let first = pick();
        assert_eq!(first.node.id, "si7-laptop");
        let c1 = claim_in(&db, &first.node.id, "me");
        let second = pick();
        assert_eq!(second.node.id, "si7-vps", "10+25 now ranks after 20");
        let c2 = claim_in(&db, &second.node.id, "me");
        let third = pick();
        assert_eq!(third.node.id, "si7-laptop", "35 < 45, and both beat 90");
        assert!(
            third.reason.contains("1 child(ren) counted"),
            "{}",
            third.reason
        );
        let c3 = claim_in(&db, &third.node.id, "me");
        assert_eq!(work_rows(&store, "si7-laptop"), 2);
        drop((c1, c2, c3));
        assert_eq!(
            work_rows(&store, "si7-laptop"),
            0,
            "finished children release"
        );
        assert_eq!(pick().node.id, "si7-laptop");
    }

    /// 306787.b item 5: a child weighs one CORE of its node, not a fixed
    /// 25 points. A 16-core laptop at 20% takes two children (20, 26, 32)
    /// before a 2-core VPS at 30% takes one; under the old +25 the VPS
    /// took the second (45 > 30). The VPS's own child then counts 50.
    #[test]
    fn a_child_weighs_one_core_of_its_node() {
        let (_dir, store, db) = scratch_at();
        reg(&store, "w-laptop", "laptop", None);
        reg(&store, "w-vps", "vps", None);
        beat(&store, "w-laptop", 20, Some(16), Some(0), None);
        beat(&store, "w-vps", 30, Some(2), Some(0), None);
        let mut held = Vec::new();
        let mut order = Vec::new();
        for _ in 0..4 {
            let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
            order.push(p.node.name.clone());
            held.push(claim_in(&db, &p.node.id, "me"));
        }
        // laptop 20 → 26 → 32 > vps 30 → vps 30+50=80 > laptop 32.
        assert_eq!(order, ["laptop", "laptop", "vps", "laptop"]);
        let rows = store.nodes_list().unwrap();
        let vps = rows.iter().find(|n| n.id == "w-vps").unwrap();
        let l = node_load(store.conn(), None, vps);
        assert_eq!(l.child_weight_x100(), 5_000);
        assert_eq!(l.projected_x100(), Some(8_000));
        let laptop = rows.iter().find(|n| n.id == "w-laptop").unwrap();
        assert_eq!(
            node_load(store.conn(), None, laptop).projected_x100(),
            Some(2_000 + 3 * 625),
            "three children at a sixteenth of the laptop each"
        );
    }

    /// IO pressure: de-prioritised in general (one point of IO stall
    /// counts like one point of load per core), worth half for CPU work,
    /// and for IO work a node at or over IO_HIGH is chosen only when no
    /// other candidate is left — after a node whose IO is unknown.
    #[test]
    fn io_pressure_is_weighed_and_high_io_is_avoided_for_io_work() {
        let (_dir, store, _db) = scratch_at();
        for (id, name) in [("io-a", "a"), ("io-b", "b"), ("io-c", "c")] {
            reg(&store, id, name, None);
        }
        beat(&store, "io-a", 5, Some(4), Some(25), None); // idle CPU, busy disk
        beat(&store, "io-b", 28, Some(4), Some(0), None); // busier CPU, idle disk
        beat(&store, "io-c", 60, Some(4), None, None); // IO unknown (Windows)
        let pick = |w: Workload, ok: &dyn Fn(&NodeRow) -> bool| {
            resolve_workload(&store, &BackendSpec::Any, None, 600, ok, &[], w)
                .unwrap()
                .node
                .name
        };
        let all = |_: &NodeRow| true;
        // general: a 5+25=30, b 28, c 60 — the busy disk costs a its lead.
        assert_eq!(pick(Workload::General, &all), "b");
        // cpu: a 2·5+25=35, b 56, c 120 — the idle CPU wins.
        assert_eq!(pick(Workload::Cpu, &all), "a");
        // io: b (low), then c (unknown) before a (high) even though a's
        // weighted score (5+2·25=55) is lower than c's (60).
        assert_eq!(pick(Workload::Io, &all), "b");
        let not_b = |n: &NodeRow| n.name != "b";
        assert_eq!(pick(Workload::Io, &not_b), "c");
        let only_a = |n: &NodeRow| n.name == "a";
        assert_eq!(
            pick(Workload::Io, &only_a),
            "a",
            "avoided, not refused: the last candidate still takes it"
        );
        let p = resolve_workload(
            &store,
            &BackendSpec::Any,
            None,
            600,
            &only_a,
            &[],
            Workload::Io,
        )
        .unwrap();
        assert!(p.reason.contains("io 25% (high)"), "{}", p.reason);
        // A named node is never moved, but says its disk is busy.
        let p = resolve_workload(
            &store,
            &BackendSpec::Node("a".into()),
            None,
            600,
            &all,
            &[],
            Workload::Io,
        )
        .unwrap();
        assert_eq!(p.node.name, "a");
        assert!(p.reason.contains("IO pressure is 25%"), "{}", p.reason);
        assert!(Workload::parse("gpu").is_err());
        assert_eq!(Workload::parse("").unwrap(), Workload::General);
        assert_eq!(Workload::parse("IO").unwrap(), Workload::Io);
    }

    /// 306787.b item 3: placements made by OTHER nodes count. A peer's
    /// heartbeat reports the children running there, by origin; this
    /// node adds only its own placements the peer has not reported yet,
    /// so a child is never counted twice — and a peer that reports
    /// nothing (0.1.6) still counts what we placed there.
    #[test]
    fn a_peers_reported_work_counts_and_ours_is_not_counted_twice() {
        let (_dir, store, db) = scratch_at();
        reg(&store, "x-me", "laptop", None);
        reg(&store, "x-vps", "vps", None);
        reg(&store, "x-old", "old", None);
        beat(&store, "x-me", 50, Some(8), Some(0), None);
        // The vps runs three children: two for another node, one for us.
        let report = WorkReport {
            running: 3,
            queued: 0,
            from: BTreeMap::from([("x-zerg".to_string(), 2), ("x-me".to_string(), 1)]),
        };
        beat(&store, "x-vps", 10, Some(4), Some(0), Some(report));
        beat(&store, "x-old", 10, None, None, None);
        let vps = || store.node_get("x-vps").unwrap().unwrap();
        // Our one child there is already in its report: 3, not 4.
        let ours = claim_in(&db, "x-vps", "x-me");
        assert_eq!(node_load(store.conn(), Some("x-me"), &vps()).children(), 3);
        // A second one it has not reported yet is added.
        let more = claim_in(&db, "x-vps", "x-me");
        let l = node_load(store.conn(), Some("x-me"), &vps());
        assert_eq!(l.unreported_from_here, 1);
        assert_eq!(l.children(), 4);
        // 10 + 4 × 25 = 110 > 50: the busy-by-others vps loses to us.
        let p = resolve(&store, &BackendSpec::Any, Some("x-me"), 600, &|n| {
            n.id != "x-old"
        })
        .unwrap();
        assert_eq!(p.node.id, "x-me", "{}", p.reason);
        // A node that reports nothing: our own placements only.
        let on_old = claim_in(&db, "x-old", "x-me");
        let old = store.node_get("x-old").unwrap().unwrap();
        let l = node_load(store.conn(), Some("x-me"), &old);
        assert_eq!((l.running, l.unreported_from_here), (None, 1));
        drop((ours, more, on_old));
    }

    /// 306787.b item 3, restart: a child's row is bounded by the child's
    /// process, not the gateway's. A restart (here: the guards leaked, as
    /// a killed process leaks them) keeps counting a child that is still
    /// running, and the boot prune drops the claim of the dead placer,
    /// the row of a child that exited, and a recycled PID.
    #[cfg(unix)]
    #[test]
    fn a_restart_keeps_running_children_and_drops_the_dead() {
        let (_dir, store, db) = scratch_at();
        reg(&store, "r-me", "laptop", None);
        // A placed child that started: its claim becomes the child's row.
        let claim = claim_in(&db, "r-me", "r-me");
        let mut sleeper = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let run = child_started_in(&db, "r-me", ChildOf::Here(Some(&claim)), sleeper.id()).unwrap();
        assert_eq!(work_rows(&store, "r-me"), 1, "taken over, not added");
        // A peer's child served here, for another origin.
        let mut served = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let peer_run = child_started_in(&db, "r-me", ChildOf::Peer("r-vps"), served.id()).unwrap();
        // A child that already exited, and a claim whose placer is gone.
        let mut done = std::process::Command::new("true").spawn().unwrap();
        done.wait().unwrap();
        let dead_row = child_started_in(&db, "r-me", ChildOf::Here(None), done.id()).unwrap();
        store
            .conn()
            .execute(
                "INSERT INTO placement_work (state, target, origin, pid, pid_start)
                 VALUES ('placed', 'r-far', 'r-me', 2147483646, NULL)",
                [],
            )
            .unwrap();
        // A live PID whose lifetime is not the one recorded.
        store
            .conn()
            .execute(
                "INSERT INTO placement_work (state, target, origin, pid, pid_start)
                 VALUES ('running', 'r-me', 'r-me', ?1, 'not-its-start')",
                rusqlite::params![i64::from(std::process::id())],
            )
            .unwrap();
        // The restart: nothing in this process releases anything.
        std::mem::forget((claim, run, peer_run, dead_row));
        let pruned = prune(store.conn(), &pid_lives).unwrap();
        assert_eq!(
            pruned,
            Pruned {
                kept: 2,
                dropped: 3
            }
        );
        let own = own_report(store.conn(), "r-me").unwrap();
        assert_eq!(own.running, 2);
        assert_eq!(own.queued, 0);
        assert_eq!(own.from.get("r-me"), Some(&1));
        assert_eq!(own.from.get("r-vps"), Some(&1));
        // The children end: the next prune drops them too.
        for c in [&mut sleeper, &mut served] {
            c.kill().unwrap();
            c.wait().unwrap();
        }
        assert_eq!(prune(store.conn(), &pid_lives).unwrap().dropped, 2);
        assert_eq!(
            own_report(store.conn(), "r-me").unwrap(),
            WorkReport::default()
        );
    }

    /// A child takes over its OWN claim and no other, so every member is
    /// counted once whatever order members start and fail in. When claims
    /// were interchangeable, B's child took A's row; A's child then
    /// failed to start, A's drop found its row running and left it, and
    /// B's untouched claim counted as queued work for the rest of B's run.
    #[cfg(unix)]
    #[test]
    fn a_child_takes_over_its_own_claim_and_no_other() {
        let (_dir, store, db) = scratch_at();
        let own = || {
            let r = own_report(store.conn(), "h-me").unwrap();
            (r.running, r.queued)
        };
        let me = std::process::id();
        let a = claim_in(&db, "h-me", "h-me");
        let b = claim_in(&db, "h-me", "h-me");
        assert_eq!(own(), (0, 2));
        // B's child starts first, and A's never does (its spawn failed).
        let rb = child_started_in(&db, "h-me", ChildOf::Here(Some(&b)), me).unwrap();
        assert_eq!(own(), (1, 1));
        drop(a);
        assert_eq!(own(), (1, 0), "A is gone; only B's running child counts");
        drop(rb);
        drop(b);
        assert_eq!(own(), (0, 0));
        // A claim its child took over is the child's: dropping the claim
        // first leaves the running row, and the child's guard removes it.
        let c = claim_in(&db, "h-me", "h-me");
        let rc = child_started_in(&db, "h-me", ChildOf::Here(Some(&c)), me).unwrap();
        drop(c);
        assert_eq!(own(), (1, 0));
        drop(rc);
        assert_eq!(own(), (0, 0));
        // An unplaced child and a peer's child each add a row of their
        // own, never one of our claims.
        let d = claim_in(&db, "h-me", "h-me");
        let unplaced = child_started_in(&db, "h-me", ChildOf::Here(None), me).unwrap();
        let peer = child_started_in(&db, "h-me", ChildOf::Peer("h-peer"), me).unwrap();
        assert_eq!(own(), (2, 1));
        let report = own_report(store.conn(), "h-me").unwrap();
        assert_eq!(report.from.get("h-peer"), Some(&1));
        assert_eq!(report.from.get("h-me"), Some(&1));
        drop((unplaced, peer, d));
        assert_eq!(own(), (0, 0));
        // A claim on another node is not this node's to take over.
        let far = claim_in(&db, "h-far", "h-me");
        let here = child_started_in(&db, "h-me", ChildOf::Here(Some(&far)), me).unwrap();
        assert_eq!(own(), (1, 0));
        assert_eq!(work_rows(&store, "h-far"), 1, "the far claim stands");
        drop((here, far));
        assert_eq!(work_rows(&store, "h-far") + work_rows(&store, "h-me"), 0);
    }

    /// A peer's figures come off the wire. A broken or hostile report
    /// (no cores, negative children, IO over 100) is clamped or read as
    /// unknown, so it cannot rank the node first.
    #[test]
    fn a_broken_report_cannot_rank_a_node_first() {
        let (_dir, store, _db) = scratch_at();
        reg(&store, "b-fine", "fine", None);
        reg(&store, "b-odd", "odd", None);
        beat(&store, "b-fine", 30, Some(4), Some(0), None);
        beat(&store, "b-odd", 30, Some(4), Some(0), None);
        store
            .conn()
            .execute(
                "UPDATE nodes SET cores = 0, children = -40, queued = -3, io_some = 250,
                        io_full = -1
                 WHERE id = 'b-odd'",
                [],
            )
            .unwrap();
        let odd = store.node_get("b-odd").unwrap().unwrap();
        let l = node_load(store.conn(), None, &odd);
        assert_eq!(l.cores, None, "zero cores is unknown");
        assert_eq!((l.running, l.queued), (Some(0), Some(0)));
        assert_eq!((l.io_some, l.io_full), (Some(100), Some(0)));
        assert_eq!(l.child_weight_x100(), 10_000 / DEFAULT_CORES);
        let p = resolve(&store, &BackendSpec::Any, None, 600, &|_| true).unwrap();
        assert_eq!(p.node.id, "b-fine", "{}", p.reason);
    }

    /// The order is a pure function of the rows: the same registry ranks
    /// the same way every time, ties by sessions served and then id.
    #[test]
    fn the_rank_is_deterministic() {
        let (_dir, store, _db) = scratch_at();
        for id in ["d-c", "d-a", "d-b"] {
            reg(&store, id, id, None);
            beat(&store, id, 40, Some(4), Some(5), None);
        }
        store.node_served_bump("d-a").unwrap();
        let order = || {
            rank(
                store.conn(),
                None,
                store.nodes_list().unwrap(),
                Workload::General,
            )
            .into_iter()
            .map(|(n, _)| n.id)
            .collect::<Vec<_>>()
        };
        let first = order();
        assert_eq!(first, ["d-b", "d-c", "d-a"]);
        for _ in 0..5 {
            assert_eq!(order(), first);
        }
    }

    fn needs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// `any` with needs skips the least loaded node when it lacks one
    /// of them, and keeps every node when needs is empty.
    #[test]
    fn any_with_needs_skips_a_node_that_lacks_one() {
        let store = scratch();
        reg_with_tools(&store, "phone-1", "phone", None, &["bash"], 0);
        reg_with_tools(
            &store,
            "lap-1",
            "laptop",
            None,
            &["bash", "rg", "podman"],
            50,
        );
        let all = |_: &NodeRow| true;
        let p =
            resolve_needing(&store, &BackendSpec::Any, None, 600, &all, &needs(&["rg"])).unwrap();
        assert_eq!(p.node.id, "lap-1");
        let p = resolve_needing(&store, &BackendSpec::Any, None, 600, &all, &[]).unwrap();
        assert_eq!(p.node.id, "phone-1", "no needs: least loaded wins");
        let err = resolve_needing(
            &store,
            &BackendSpec::Any,
            None,
            600,
            &all,
            &needs(&["agent-browser"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("agent-browser"), "{err}");
    }

    /// `gpu` with needs: only GPU nodes that have them all.
    #[test]
    fn gpu_with_needs_filters_by_tools() {
        let store = scratch();
        let card = Some(r#"{"name":"RTX"}"#);
        reg_with_tools(&store, "g1", "rig", card, &["bash"], 0);
        reg_with_tools(&store, "g2", "laptop", card, &["bash", "python3"], 70);
        let all = |_: &NodeRow| true;
        let p = resolve_needing(
            &store,
            &BackendSpec::Gpu,
            None,
            600,
            &all,
            &needs(&["python3"]),
        )
        .unwrap();
        assert_eq!(p.node.id, "g2");
        let err = resolve_needing(&store, &BackendSpec::Gpu, None, 600, &all, &needs(&["jq"]))
            .unwrap_err();
        assert!(err.to_string().contains("jq"), "{err}");
    }

    /// A named node lacking a needed program is an error naming it —
    /// never a quiet move to a node that has it. One that advertises
    /// nothing (an older build) cannot confirm and is refused too.
    #[test]
    fn a_named_node_lacking_a_need_is_an_error_naming_it() {
        let store = scratch();
        reg_with_tools(&store, "phone-1", "phone", None, &["bash"], 0);
        reg_with_tools(&store, "lap-1", "laptop", None, &["bash", "rg"], 50);
        reg(&store, "old-1", "old", None);
        let all = |_: &NodeRow| true;
        let named = |n: &str| BackendSpec::Node(n.into());
        let err = resolve_needing(
            &store,
            &named("phone"),
            None,
            600,
            &all,
            &needs(&["bash", "rg"]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("phone") && err.contains("does not have rg"),
            "{err}"
        );
        assert!(!err.contains("have bash"), "only what is missing: {err}");
        let p =
            resolve_needing(&store, &named("laptop"), None, 600, &all, &needs(&["rg"])).unwrap();
        assert_eq!(p.node.id, "lap-1");
        assert!(resolve_needing(&store, &named("old"), None, 600, &all, &needs(&["rg"])).is_err());
        assert!(resolve_needing(&store, &named("old"), None, 600, &all, &[]).is_ok());
        let err = resolve_needing(
            &store,
            &BackendSpec::Local,
            Some("phone-1"),
            600,
            &all,
            &needs(&["rg"]),
        )
        .unwrap_err();
        assert!(err.to_string().contains("rg"), "{err}");
    }
}
