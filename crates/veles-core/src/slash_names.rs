//! The built-in slash-command names, per surface — the taxonomy the
//! command dispatcher (`veles_frontends::commands`) implements and the
//! engine consults: `aliases::is_builtin` refuses an alias that would
//! shadow one of these. Moved out of the dispatcher 2026-09-28, when the
//! frontends became their own crate; `veles_frontends::commands`
//! re-exports all three, and the reachability tests stay there with the
//! handlers.

/// Commands implemented here, reachable from the REPL, the TUI and the
/// gateway (and through it, IRC and Telegram). Every name in this list
/// is asserted to be reachable from all three; see the tests at the
/// bottom of the file.
pub const SHARED_COMMANDS: &[&str] = &[
    "/plan",
    "/mech",
    "/mechanisms",
    "/acl",
    "/access",
    // `/acl`'s three top-level spellings, which IRC had as `.agent
    // caps|grant|revoke` before the sigil fold. Kept as names rather
    // than folded away: they were the muscle memory, and a command that
    // exists on one surface only is exactly what this table is for
    // eliminating.
    "/caps",
    "/grant",
    "/revoke",
    // The pending questions. Colloquial "1"/"yes" still resolves a
    // chat's oldest ask in the adapter; these are for a channel with
    // several in flight.
    "/asks",
    "/answer",
    "/approve",
    "/deny",
    // The ACL's two neighbours, new in P7: the named sandboxes and the
    // per-role context ceilings, manageable from every surface rather
    // than only from config.toml (USER: "ALL of this should be
    // manageable not only through the config.toml file, but with full
    // first-class support in all frontends").
    "/sandbox",
    "/sandboxes",
    "/limits",
    // The emergency brake (`lockdown.rs`): engageable by an admin from
    // anywhere, liftable only from a local surface.
    "/lockdown",
    "/body",
    // Where this session's web traffic leaves (egress.rs): the owner's
    // address, or the way other people's goes.
    "/egress",
    "/crons",
    "/cron",
    // Triggers (plan item 1, 2026-09-14): reactions to channel events
    // that run without a turn — slopdrop pass-through, vision, prompt,
    // command. AclCtx-shaped like /acl and /limits: no LLM session is
    // involved in listing or editing them. The async half is
    // `/triggers refresh`/`mask`, which talks to slopdrop over HTTP.
    "/triggers",
    "/trigger",
    "/cancel",
    "/stop",
    // /cancel's gentle sibling: move the running foreground `terminal`
    // command into the background shells instead of killing it. Flag-
    // armed like cancel (reaches the running turn without the agent
    // lock), answered by the tool's own wait loop.
    "/detach",
    // The running-work families, split by kind (USER taxonomy,
    // 2026-09-17): `/tasks` is the TODO list — it always meant that on
    // the desktop pane, and the chat command meaning something else by
    // the same name was the confusion this split ends. Background
    // SHELLS start with `/background <cmd>` and list with `/shells`;
    // delegated children list under `/subagents` and start from chat
    // with `/btw`; condition wakeups list under `/monitors` (only the
    // model starts those — the monitor tool).
    "/background",
    "/bg",
    "/btw",
    "/subagents",
    "/monitors",
    // Evaluating Tcl for the caller — the IRC sigil answered it before
    // the bus from the start, but the literal `/tcl` spelling had no
    // tier and no arm anywhere, so on every other surface it reached the
    // MODEL as a prompt and cost a billed turn to compute `expr {1+1}`.
    // USER, 2026-09-11: "make sure that .tcl|/tcl bypass the llm so that
    // they don't consume a turn for a simple request from a dumb
    // machine." Shared, so both spellings and every surface agree.
    "/tcl",
    "/eval",
    // slopdrop's `tclAdmin`, by its own name. Separate command rather
    // than a role check on `/tcl`, because that is how slopdrop works
    // and because collapsing them made plain `tcl` fail for the owner
    // and for nobody else.
    "/tcladmin",
    // One-shot Tcl LSP from chat (2026-09-12 plan §2). Same tier as
    // /tcl — it names a host file — and shared so every surface
    // answers both spellings identically.
    "/lint",
    "/fmt",
    // Media generation as a first-class act (image/video/audio) — the
    // model has the tools, but "draw me a cat" should not need a turn
    // that might not call one. USER, 2026-09-11: commands "dedicated
    // to image/video/audio generation letting the user specify the
    // provider and/or model out of available ones, gated behind user+
    // acls". (`/image` was taken — it attaches a file to the next
    // prompt — so this is `/gen`.)
    "/gen",
    "/generate",
    // ── moved in from DUPLICATED_ACROSS_SURFACES (RM4.3) ─────────────
    // The readers and the session-state writers first; /model is the
    // one the plan calls the actual fix for "IRC has no /model".
    "/model",
    "/models",
    "/defaults",
    // Config changes a peer sent that wait for the owner (a security
    // section, or a conflict) — `configsync_auto` applies the rest.
    "/configsync",
    "/reasoning",
    "/usage",
    // The compute-pool registry view (B3a) — same table everywhere,
    // grown by gossip.
    "/nodes",
    // The UPSTREAM plan, which /usage is not: `/usage` is the ledger
    // this agent writes, `/quota` is what Alibaba says is left. Async
    // (HTTP), so it has an awaited twin and a GATEWAY_OVERRIDES entry.
    "/quota",
    "/context",
    "/status",
    "/version",
    "/tools",
    "/guards",
    "/checkpoints",
    "/restore",
    "/todo",
    "/todos",
    "/plugins",
    "/branch",
    "/fork",
    "/export",
    "/diff",
    "/image",
    "/attach",
    "/tasks",
    "/shells",
    "/profile",
    "/skills",
    "/memory",
    "/commands",
    "/agents",
    "/failover",
    "/sessions",
    "/steer",
    "/title",
    "/secret",
    // The hook taxonomy, visible as a command (the desktop has a pane
    // for it; this is the same report for every other surface).
    "/hooks",
    // The honest no-op: config, commands and skills are read from disk
    // per use, so there is nothing to reload.
    "/reload",
    // The shared message queue: text parked while a turn ran, visible
    // and cancellable from every surface (R4.6).
    "/queue",
    // Session migration (B6): move/reclaim/own across pool nodes. The
    // arm has been in `dispatch` since the migration landed; without
    // the registration it would have a dispatcher and an override but
    // no ACL tier, no help row, no Telegram menu entry and no desktop
    // palette row — exactly the drift the parity tests exist to catch.
    "/session",
    // The transcript editors — the family that needed CommandCtx.
    "/clear",
    "/undo",
    "/compress",
    "/compact",
    // The UI language, shared so every surface can answer it. The arm
    // has been in `dispatch` since the swap landed, but the name was in
    // NO list — which is why the parity test above only caught it now:
    // a command can be live on four surfaces and still be missing its
    // ACL tier, its help row, its Telegram menu entry and its desktop
    // palette row.
    "/locale",
    "/language",
    "/lang",
];

/// Commands that belong to ONE surface because there is nothing to
/// share: they act on the rendering, the terminal or the process, not on
/// the agent. Listed so the parity test can tell "deliberately local"
/// from "forgotten", which is the distinction the old three-way split
/// could not express.
pub const SURFACE_SPECIFIC: &[&str] = &[
    // TUI: rendering and panes.
    "/theme",
    "/themes",
    "/bindings",
    "/art",
    "/log",
    "/logs",
    "/files",
    // REPL: the terminal itself.
    "/copy",
    "/paste",
    "/exit",
    "/quit",
    "/redraw",
    // Gateway: the owner joins someone else's session from one of their
    // chats (`veles_core::join`). Only a chat surface holds other
    // people's sessions.
    "/join",
    "/leave",
];

/// The THIRD category, and the honest one: engine-state commands that
/// `chat.rs` and `tui.rs` each implement separately, in their own code,
/// today.
///
/// Found 2026-09-04 by asking a question the existing tests did not:
/// they enforce that every *declared* shared command reaches all three
/// surfaces, and say nothing about a command that was simply never
/// declared. Thirty-one were in exactly that position — most of the
/// engine-state surface — so the dispatcher was policing the handful
/// that had been moved while divergence continued everywhere else.
///
/// RM4.3 (2026-09-07) moved thirty of them into `dispatch` in one
/// sweep. What is left, deliberately:
///
/// - `/help` — three hand-maintained texts that genuinely disagree
///   about what their surface can do. R4.4 replaces all three with one
///   generated listing; until then, moving one text would lie about
///   two surfaces.
/// - `/new` — needs the `AgentHandle` (rebind swaps the Arc, which a
///   `&Agent` cannot do), not the agent.
/// - `/resume`, `/retry` (RM4.3c) — the `/new` family: resuming
///   rebinds the handle (REPL `rebind`, TUI `switch_to`) and retry
///   runs a turn through surface machinery the REPL and the TUI do not
///   share (`run_prompt_once` vs a spawned `launch_turn`). Same shape,
///   same engine; only the handle plumbing differs.
pub const DUPLICATED_ACROSS_SURFACES: &[&str] = &["/help", "/new", "/resume", "/retry"];
