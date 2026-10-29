//! The commands every text surface shares, implemented once.
//!
//! USER (2026-09-03), on the audit that produced this: *"we have a good
//! routine established for auditing now — don't believe the docstrings,
//! documentation or comments, make sure the implementation is real and
//! wired up to frontends."*
//!
//! WHAT THE AUDIT FOUND
//!
//! Nothing fake. What it found instead was **reachability**: the REPL had
//! 48 slash commands, the TUI 43, the gateway ~15, and seventeen of the
//! REPL's were REPL-only. `Agent::mechanisms_listing` — "what the engine
//! may do on its own, and how to stop it" — had exactly ONE caller, in
//! `chat.rs`. Plan mode was enforceable everywhere and switchable in
//! three places out of five. The gateway could list and cancel background
//! tasks it had no way to create.
//!
//! None of that was a decision. It is what three hand-maintained `match`
//! statements do to each other over time, and copying arms between them
//! again would buy a few weeks before the next divergence.
//!
//! THE SPLIT
//!
//! A command that reads or changes ENGINE state lives here and is
//! reachable from every text surface. A command that manipulates the
//! SURFACE — rendering (`/theme`, `/art`, `/redraw`, `/log`), the
//! terminal's clipboard (`/copy`, `/paste`), the process (`/exit`,
//! `/quit`) — stays in the frontend that owns it, because there is
//! nothing to share.
//!
//! `SHARED_COMMANDS` and `SURFACE_SPECIFIC` are the written form of that
//! rule, and `every_shared_command_reaches_every_surface` is the enforced
//! form. That pairing is the `hooks.rs` taxonomy idiom (`KNOWN_HOOKS` =
//! `FIRED` ⊎ `RESERVED`, checked by `include_str!` over the sources that
//! must contain the call): a rule nothing checks is a comment.
//!
//! WHY `CommandCtx` AND NOT A BARE AGENT
//!
//! The transcript-editing commands (`/clear`, `/undo`, `/compress`)
//! need the conversation history, and they need to know whether a turn
//! is running — refusing to race a live turn is the deliberate
//! decision the old `chat.rs` made by accident when it dropped the
//! agent guard before touching history. The lock order, fixed by C1's
//! read-guard turn, is now stated once here: agent first, history
//! after, always.
//!
//! The `principal` arrives with the gateway, where the sender is not
//! necessarily the owner; owner surfaces pass `None` and get the
//! owner's answer.

use veles_core::agent::Agent;
use veles_core::lockutil::{LockOk, RwLockOk};

/// Which surface is asking. Output phrasing and the few genuinely
/// surface-shaped behaviours key off this; the COMMANDS do not.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    Repl,
    Tui,
    /// The Tk desktop, over its HTTP API. Answers like the gateway in
    /// every visible way — it renders the same table and takes the same
    /// arms — but it is a LOCAL surface, and `/lockdown off` is the one
    /// place that difference decides something. It used to pass
    /// `Surface::Gateway`, which made "is this caller local" an
    /// unanswerable question.
    Desktop,
    Gateway,
}

impl Surface {
    /// Whether reaching this surface already means being at the
    /// machine veles runs on.
    ///
    /// Not the same question as "is there a principal": cron and A2A
    /// carry none either, and neither is a person at a keyboard. The
    /// only evidence that counts here is the surface itself.
    pub fn is_local(self) -> bool {
        matches!(self, Surface::Repl | Surface::Tui | Surface::Desktop)
    }
}

/// Everything a shared command may need, and nothing it may not.
pub struct CommandCtx<'a> {
    pub agent: &'a Agent,
    pub surface: Surface,
    /// The live conversation, for the transcript editors. `None` on
    /// the gateway, which loads history from the store per turn.
    pub history: Option<&'a mut Vec<veles_core::providers::ChatMessage>>,
    /// True while a turn is running in this session. The transcript
    /// editors refuse with an honest message rather than racing it —
    /// the decision `chat.rs`'s old comment deferred, made on purpose.
    pub turn_live: bool,
    /// The resolved sender, when the surface has one that is not the
    /// owner.
    pub principal: Option<&'a veles_core::acl::Principal>,
}

/// A dispatched command's result.
#[derive(Clone, Debug)]
pub struct Handled {
    pub text: String,
    /// The session's transcript changed — surfaces with a local copy
    /// (the TUI) rebuild it from the store.
    pub reload: bool,
}

impl From<&str> for Handled {
    fn from(text: &str) -> Self {
        Handled {
            text: text.to_string(),
            reload: false,
        }
    }
}

impl From<String> for Handled {
    fn from(text: String) -> Self {
        Handled {
            text,
            reload: false,
        }
    }
}

pub use veles_core::slash_names::{DUPLICATED_ACROSS_SURFACES, SHARED_COMMANDS, SURFACE_SPECIFIC};

/// Shared commands the GATEWAY deliberately answers itself.
///
/// Found while wiring, not designed up front: `/acl` on a messaging
/// surface must answer "what may THIS sender do", because the sender is
/// not necessarily the owner. The REPL and the TUI are owner surfaces
/// where that question has one answer, so they want the owner-surface
/// listing here instead. Two correct implementations of one name, which
/// is a third category and not a bug — but only as long as it is written
/// down and checked, hence this list and its test.
///
/// `/status` and `/compress`|`/compact` ride the same rule: the
/// gateway's status is ITS lifecycle (what a messaging user is asking
/// about), and compaction there is `.await`ed in async context rather
/// than block_on'd (RM4.3).
///
/// The gateway's shared dispatch runs AFTER its builtin table, so the
/// override happens by ordering rather than by a special case.
pub const GATEWAY_OVERRIDES: &[&str] = &[
    "/acl",
    "/access",
    "/cancel",
    "/stop",
    // /detach is answered in the gateway's pre-lock fast lane, same as
    // /cancel and for the same reason: a command that must reach a turn
    // blocked INSIDE a foreground terminal call cannot take the agent
    // lock that turn is holding.
    "/detach",
    "/status",
    "/compress",
    "/compact",
    // `/sandbox` is async (podman); the gateway is already in an async
    // context and awaits it, where the owner surfaces block on a fresh
    // runtime. Same implementation, two call styles — which is the
    // reason this list exists.
    "/sandbox",
    "/sandboxes",
    // `/gen` is async the same way (generation is HTTP); the gateway
    // awaits it, the owner surfaces block on a fresh runtime.
    "/gen",
    "/generate",
    // `/tcl` is async for the same reason (a subprocess, and an HTTP
    // call once the slopdrop bridge lands); the gateway awaits it.
    "/tcl",
    "/eval",
    "/tcladmin",
    // `/triggers` is async on the gateway for the same reason as /tcl:
    // `refresh`/`mask` talk to slopdrop over HTTP, and the gateway is
    // already in an async context. The sync dispatcher answers every
    // other verb, so only the HTTP half needs the override.
    "/triggers",
    "/trigger",
    // `/quota` reads the Alibaba account over HTTP, same shape.
    "/quota",
    // `/models`: discovery may fetch a provider's catalog over HTTP.
    "/models",
    // `/session migrate|reclaim` is async (the A2A push/query); the
    // gateway awaits it rather than blocking a fresh runtime inside
    // its own. Same implementation, two call styles.
    "/session",
];

/// The TUI's sanctioned overrides. Two kinds, both listed for the same
/// reason GATEWAY_OVERRIDES is: an unlisted divergence is a bug, a
/// listed one is a decision.
///
/// - `/compress`|`/compact` run there as a spawned worker
///   (`spawn_compact`) so the summarizer call never blocks the
///   single-threaded UI loop — same command, same engine path
///   (`compact_forced`), different execution shape.
/// - The "editors, not dumps" family: a bare command whose result is a
///   choice opens a modal picker the user acts in (switch model, restore
///   a checkpoint, view a skill/hook/plugin/diff/memory/acl) instead of
///   dumping prose into the transcript. A specific argument (`/model x`,
///   `/diff staged`) still reaches the shared dispatcher arm below.
pub const TUI_OVERRIDES: &[&str] = &[
    "/compress",
    "/compact",
    "/model",
    "/checkpoints",
    "/skills",
    "/plugins",
    "/hooks",
    "/memory",
    "/acl",
    "/diff",
];

/// The ONE help table (RM4.4). Until now the REPL, the TUI and the
/// gateway each hand-maintained a listing, and the three disagreed —
/// the TUI's forgot `/resume` existed, the REPL's described `/reload`
/// as reloading something, the gateway's named five commands out of
/// thirty-nine. Every surface renders THIS table, filtered by what
/// the surface adds locally; a command missing from the table is now a
/// test failure (`every_surface_help_names_every_shared_command`),
/// not a documentation drift.
pub fn help_entries(surface: Surface) -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&'static str, &'static str)> = vec![
        (
            "/model [name]",
            "show or switch model (alias | model@provider | custom:<prov>)",
        ),
        (
            "/models [provider] [text]",
            "list a provider's models (default: this session's provider); add text to search them",
        ),
        ("/reasoning [lvl]", "effort level"),
        ("/status", "session, model, lifecycle"),
        ("/version", "which build this is"),
        (
            "/locale [id|swap|reset|primary <id>|secondary <id>]",
            "UI language: list the catalogs, set this session's language (the process on the TUI/REPL), or write the primary/secondary pair (admin)",
        ),
        ("/usage", "token usage by provider"),
        (
            "/nodes",
            "compute-pool registry: nodes, liveness, singleton arbiter",
        ),
        (
            "/session migrate <id> <node>|reclaim <id>|owner <id>",
            "move a session (and its subtree) to another pool node at a turn boundary; finish/undo a parked handoff; ask who owns a session (admin)",
        ),
        (
            "/quota [team|personal|zai|kimi|spend|refresh]",
            "upstream plan credits and windows — /usage is the local ledger",
        ),
        ("/context", "prompt breakdown"),
        ("/profile", "active profile"),
        ("/sessions", "list sessions"),
        ("/title [name]", "name this session"),
        ("/resume <id>", "resume a session (prefix ok)"),
        ("/retry", "resend the last prompt"),
        ("/undo [N]", "back up N user turns"),
        ("/checkpoints", "file edits you can undo"),
        ("/restore <id>", "restore a checkpoint"),
        ("/branch [name]", "fork this session"),
        ("/clear", "drop context"),
        ("/compress", "archive old messages"),
        ("/steer <msg>", "redirect mid-turn"),
        ("/cancel", "stop the running turn"),
        (
            "/detach",
            "move the running foreground command to the background",
        ),
        (
            "/plan [on|off]",
            "research freely, change nothing · /plan show|tasks|status [session]",
        ),
        ("/mech [name val]", "engine mechanisms + toggles"),
        ("/body [json|clear]", "session extra_body override"),
        (
            "/egress [tor|home]",
            "this session's searches and browser: the home connection, or Tor as for other users",
        ),
        ("/failover [mode]", "ask | auto | stop | script"),
        ("/tasks", "the TODO list (same as /todos)"),
        ("/shells", "background shells; /shells output|kill <id>"),
        ("/background <cmd>", "run a command as a background shell"),
        (
            "/btw <prompt>",
            "fork a one-off subagent off this session; answer returns here",
        ),
        ("/subagents", "running subagents; /subagents cancel <id>"),
        (
            "/monitors",
            "condition wakeups the model set; /monitors cancel <id>",
        ),
        (
            "/gen <img|vid|audio> [route] <p>",
            "make media: image/video/audio, route = comfyui or provider[/model]",
        ),
        (
            "/tcl <code>",
            "evaluate Tcl and answer with the result — no model turn",
        ),
        (
            "/tcladmin <code>",
            "the same, on the privileged path (owner only)",
        ),
        (
            "/lint <file>",
            "one-shot tcl-lsp diagnostics — no model turn",
        ),
        (
            "/fmt <file> [check]",
            "format a Tcl file via tcl-lsp; check = dry run",
        ),
        ("/tools", "list tools"),
        ("/guards", "harness guard state"),
        ("/hooks", "hook taxonomy + who listens"),
        ("/skills", "list skills"),
        ("/memory", "memory stats"),
        (
            "/plugins [reload|stop|start <name[:instance]>]",
            "list plugins",
        ),
        (
            "/commands",
            "custom commands (.veles/commands/) and config aliases",
        ),
        ("/agents", "defined subagent profiles (.veles/agents/)"),
        ("/todos", "the TODO list (same as /tasks)"),
        ("/diff [mode]", "git changes"),
        ("/export [md|json]", "export session"),
        ("/image <path>", "attach an image or file"),
        (
            "/crons",
            "scheduled jobs; /crons add \"<sched>\" [session=<id>] <prompt> (alias /cron)",
        ),
        (
            "/triggers",
            "channel-event reactions; /triggers add <name> network=… channel=… action=…",
        ),
        (
            "/defaults [what] [v]",
            "routes NEW sessions start on; /model is this session only",
        ),
        (
            "/configsync [apply|drop]",
            "config changes from peers that wait for you; /configsync apply lands them",
        ),
        (
            "/acl",
            "who you are and what you may do; explain/grant/revoke/promote (admin)",
        ),
        (
            "/sandbox",
            "where your tools run; list/create/assign/purge (admin)",
        ),
        ("/limits", "context ceilings per role; set (admin)"),
        (
            "/lockdown [on|off]",
            "refuse every connector; lift only from a local frontend",
        ),
        ("/caps", "your own role and capabilities"),
        (
            "/grant <id> <role>",
            "grant a role (owner) · /revoke <id> takes it back",
        ),
        (
            "/asks",
            "questions waiting on an answer · /approve|/deny|/answer <id>",
        ),
        ("/secret …", "secrets, redacted from history (admin)"),
        ("/reload", "(nothing to reload — read from disk per use)"),
        ("/queue", "messages parked mid-turn; cancel/edit <seq>"),
    ];
    match surface {
        Surface::Repl => v.extend([
            ("/new", "fresh session"),
            ("/quit | /exit", "exit"),
            ("/copy", "copy last reply"),
            ("/paste", "send clipboard as prompt"),
            ("/redraw", "(CLI redraws automatically)"),
        ]),
        Surface::Tui => v.extend([
            ("/new", "fresh session (Ctrl-N too)"),
            ("/quit | /exit", "leave (cancels a running turn first)"),
            ("/theme [name]", "switch palette · /themes lists all"),
            ("/bindings [name]", "keymap: normie/emacs/vi"),
            ("/log", "toggle the log pane (F2)"),
            ("/files [path]", "browse the filesystem"),
            ("/art", "the synthwave splash"),
        ]),
        Surface::Gateway | Surface::Desktop => {
            v.insert(0, ("/pair [code]", "pair a desktop with this agent"));
            if matches!(surface, Surface::Gateway | Surface::Desktop) {
                v.extend([
                    (
                        "/join [name|id]",
                        "join someone's session: what you write goes there and runs with your rights",
                    ),
                    ("/leave", "stop writing into the session you joined"),
                ]);
            }
        }
    }
    v
}

/// `help_entries` with each description translated for the active
/// locale.
///
/// The table itself stays English `&'static str` — it is the SOURCE
/// (the key convention is "English text is the key"), and the parity
/// tests assert its shape rather than its spelling. Every site that
/// RENDERS a description goes through here, so a translated catalog
/// reaches the REPL's Tab-completion, the TUI palette, the desktop's
/// `/api/commands/builtin` and Telegram's `/` menu from one place.
/// A description with no entry in the active locale comes back as the
/// English text, which is what msgcat does on the desktop side too.
pub fn help_entries_localized(surface: Surface) -> Vec<(String, String)> {
    help_entries(surface)
        .into_iter()
        .map(|(usage, desc)| (usage.to_string(), veles_core::i18n::lookup(desc, &[])))
        .collect()
}

/// Same for one description — the render sites that hold a raw row.
pub fn desc_localized(desc: &str) -> String {
    veles_core::i18n::lookup(desc, &[])
}

/// `help_entries` as text — the REPL's `/help`, the TUI's `/help` and
/// the gateway's `help` builtin all print exactly this shape.
pub fn help_text(surface: Surface) -> String {
    help_text_for(surface, None)
}

/// The same table, honest about what the CALLER can actually run.
///
/// Discovery was open while execution was gated: a guest in a channel
/// was shown `/grant`, `/lockdown` and `/sandbox purge` alongside
/// `/status`, and learned which half was real by being refused one at
/// a time. Rows whose capability this principal lacks are dropped, and
/// the number dropped is STATED — "there are commands you cannot run"
/// is true and worth knowing, while a menu of them is only noise, and
/// a silent shortening would be the invisible mechanism the transcript
/// rules exist to prevent.
///
/// `None` is a local owner surface (the REPL, the TUI, the desktop, the
/// CLI): no principal, nothing filtered. The desktop annotates instead
/// of filtering — it serves the whole table with each row's cap at
/// `GET /api/commands/builtin`, because a palette that hides rows from
/// the machine's owner is a worse lie than one that greys them.
pub fn help_text_for(surface: Surface, principal: Option<&veles_core::acl::Principal>) -> String {
    let mut rows: Vec<String> = Vec::new();
    let mut withheld = 0usize;
    for (usage, desc) in help_entries(surface) {
        let name = usage
            .split_whitespace()
            .next()
            .unwrap_or(usage)
            .trim_start_matches('/');
        match veles_core::acl::builtin_cap(name) {
            Some(cap) if !caller_is(principal, cap) => withheld += 1,
            // The DESCRIPTION is translated; the usage column is
            // syntax (a command name and its argument names), and
            // translating it would produce a command nobody can type.
            _ => rows.push(format!("  {usage:<22} {}", desc_localized(desc))),
        }
    }
    if withheld > 0 {
        rows.push(format!(
            "  ({withheld} more need a capability you do not have)"
        ));
    }
    rows.join("\n")
}

/// The FOURTH category: commands an adapter keeps legitimately local.
///
/// IRC's `.agent asks|answer` (and the prefix-syntax set around them —
/// `caps|grant|revoke|tcl|nick` — which the slash-scanner cannot see
/// and which is the adapter's own surface, not a copy of one) never go
/// through the bus, so they cannot share an implementation with the
/// text surfaces. Telegram's pre-bus intercepts (`/todos`,
/// `/checkpoints`, `/restore`) are NOT this: they duplicate text-surface
/// commands and are declared as debt in DUPLICATED_ACROSS_SURFACES,
/// where they belong. This list exists so "deliberately local" stays a
/// written decision, the same distinction SURFACE_SPECIFIC draws for
/// the in-process surfaces.
pub const ADAPTER_LOCAL: &[&str] = &["/asks", "/answer"];

/// Run a shared command.
///
/// `None` means "not mine" — the caller falls through to its own
/// surface-specific arms. `Some(Handled)` means handled; errors are
/// formatted into the text rather than returned, because every caller
/// did exactly that with its own `Err` arm and three copies of
/// `format!("error: {e}")` is two too many.
pub fn dispatch(ctx: &mut CommandCtx, head: &str, rest: &str) -> Option<Handled> {
    let agent = ctx.agent;
    // Every answer below renders in THIS session's language (or the
    // request's, e.g. the desktop window's), not the process's: the
    // gateway answers many users on many surfaces from one process.
    let _locale = veles_core::i18n::ScopeGuard::enter(agent.request_locale());
    match head {
        // Plan mode. The gateway intercepts this before its own
        // dispatcher because every messaging adapter routes through one
        // place; the REPL and the TUI run their agent in-process and so
        // need a real arm. This is that arm, once.
        //
        // The FIRST WORD may be a read verb (`show`, `tasks`, `status`)
        // rather than a mode word: `on`/`off` MOVE the session and
        // announce the move to the transcript, which a read must not.
        // The verb gets the first look and `Verb::parse` claims no
        // spelling `Mode::parse` claims (pinned by
        // `plan::tests::the_verb_table_and_the_mode_table_are_disjoint`),
        // so `/plan off` can never be read as a question. An optional
        // second word names another session to read (the same resolver
        // the `show_plan` tool uses). The renderer is the tool's, so
        // `/plan show` and a model's call answer the same characters.
        "/plan" => {
            let arg = rest.trim();
            let (word, tail) = match arg.split_once(char::is_whitespace) {
                Some((w, t)) => (w, t.trim()),
                None => (arg, ""),
            };
            match veles_core::plan::Verb::parse(word) {
                Some(verb) => {
                    // A named session is a read of a conversation that
                    // is not the caller's — same capability the tool's
                    // `session` argument pays, same shared denial, so
                    // the two doors cannot disagree about who may, or
                    // about what counts as "another session". Checked
                    // BEFORE resolution: an ambiguity error would itself
                    // answer a question about sessions outside the
                    // caller's reach.
                    if let Some(d) = veles_core::plan::cross_session_read_denial(tail, ctx.principal) {
                        return Some(d.into());
                    }
                    let store = agent.store.lock_ok();
                    let text =
                        match veles_core::plan::resolve_read_target(&store, tail, &agent.session_id) {
                            Ok(target) => veles_core::plan::render_state(
                                &store,
                                &target,
                                &agent.session_id,
                                verb,
                            ),
                            Err(e) => format!("error: {e:#}"),
                        };
                    Some(text.into())
                }
                None => Some(one_or_error(agent.set_plan_mode(opt(rest))).into()),
            }
        }

        // Session `extra_body` override — provider knobs that no config
        // key covers, per session.
        "/body" => Some(one_or_error(agent.set_extra_body(opt(rest))).into()),

        // Where this session's searches, fetches and browser leave
        // (egress.rs): the owner's own address by default, or the way
        // other people's go, to see what they see (USER, 2026-10-13: "I
        // should still be able to run through sandbox/tor to debug other
        // users' requests").
        "/egress" => Some(
            one_or_error(veles_core::egress::command(
                &agent.store.lock_ok(),
                &agent.session_id,
                opt(rest),
            ))
            .into(),
        ),

        // What the engine may do WITHOUT being asked, and how to stop
        // each one. This existed only in the REPL, which is the wrong
        // place for it: the surfaces where an agent acts unattended are
        // the gateway and the desktop.
        "/mech" | "/mechanisms" => Some(if rest.is_empty() {
            agent.mechanisms_listing()
        } else {
            let mut parts = rest.split_whitespace();
            let name = parts.next().unwrap_or("");
            one_or_error(agent.set_mechanism(name, parts.next()))
        }
        .into()),

        // Who am I and what may I do. The owner surfaces answer "full
        // caps" and then show the grant table, which is the part that
        // actually varies.
        "/acl" | "/access" => {
            Some(acl_command(&AclCtx::from_agent(agent, ctx.principal), rest).into())
        }

        // The context ceilings, readable by the person they bind and
        // settable by an admin — one implementation, every surface.
        "/limits" => {
            Some(limits_command(&AclCtx::from_agent(agent, ctx.principal), rest).into())
        }

        // `/acl`'s top-level spellings. One implementation, three
        // names, so `/grant` cannot drift from `/acl grant`.
        "/caps" => Some(acl_command(&AclCtx::from_agent(agent, ctx.principal), "caps").into()),
        "/grant" | "/revoke" => Some(
            acl_command(
                &AclCtx::from_agent(agent, ctx.principal),
                &format!("{} {rest}", head.trim_start_matches('/')),
            )
            .into(),
        ),

        // The pending questions. An owner surface sees every open ask;
        // the gateway scopes to the chat and answers as the sender,
        // which the adapters do themselves (they must answer while a
        // turn is RUNNING — see IRC_IMMEDIATE).
        "/asks" | "/answer" | "/approve" | "/deny" => Some(
            ask_command(
                ctx.principal,
                None,
                if ctx.surface.is_local() {
                    veles_core::ask::Decider::LocalOwner
                } else {
                    match ctx.principal {
                        Some(p) => veles_core::ask::Decider::principal(p),
                        None => veles_core::ask::Decider::sender(None),
                    }
                },
                head.trim_start_matches('/'),
                rest,
            )
            .into(),
        ),

        // The emergency brake. The SURFACE is an argument here because
        // it decides something: `off` is refused from a connector,
        // whatever the caller's role.
        "/lockdown" => Some(
            lockdown_command(
                &AclCtx::from_agent(agent, ctx.principal),
                ctx.surface,
                rest,
            )
            .into(),
        ),

        // Where this session's tools run, and the named boxes. Async
        // (podman) — the gateway awaits it in `GATEWAY_OVERRIDES`; here
        // it blocks, the way `/compress` does.
        "/sandbox" | "/sandboxes" => Some({
            let rest = if head == "/sandboxes" && rest.trim().is_empty() {
                "list"
            } else {
                rest
            };
            match tokio::runtime::Runtime::new() {
                Ok(rt) => rt
                    .block_on(sandbox_command(&AclCtx::from_agent(agent, ctx.principal), rest))
                    .into(),
                Err(e) => format!("runtime: {e}").into(),
            }
        }),

        // Media generation, straight from the human. Same async shape
        // as /sandbox: the gateway awaits it, we block.
        "/gen" | "/generate" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => rt
                .block_on(gen_command(&AclCtx::from_agent(agent, ctx.principal), rest))
                .into(),
            Err(e) => format!("runtime: {e}").into(),
        }),

        // Tcl, evaluated for the caller — NO turn, no provider call.
        // The IRC sigil (`.tcl`) has always been answered before the
        // bus; this is the same command reached by its other spelling,
        // and having it here is what keeps `/tcl expr {1+1}` from being
        // handed to a model as a prompt on every other surface.
        "/tcl" | "/eval" | "/tcladmin" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => rt
                .block_on(tcl_command_as(
                    &AclCtx::from_agent(agent, ctx.principal),
                    rest,
                    head == "/tcladmin",
                ))
                .into(),
            Err(e) => format!("runtime: {e}").into(),
        }),

        // One-shot Tcl LSP from chat (2026-09-12 plan §2): diagnostics
        // and formatting of a FILE the caller names, no model turn.
        // Own short-lived child (never lsp_proxy::start — it would
        // evict the desktop's live Dev-pane session); see lsp_check.rs.
        // Shell tier, same as /tcl: it names a host path.
        "/lint" => Some({
            let out: Handled = if !caller_is(ctx.principal, veles_core::acl::Cap::Shell) {
                denied(ctx.principal, "/lint", veles_core::acl::Cap::Shell).into()
            } else if rest.trim().is_empty() {
                "usage: /lint <file.tcl>  — one-shot tcl-lsp diagnostics, no model turn".into()
            } else {
                let path = rest.split_whitespace().next().unwrap_or("");
                match veles_core::lsp_check::lint_file(path, 30) {
                    Ok(diag) if diag.is_empty() => {
                        format!("{path}: clean — no diagnostics").into()
                    }
                    Ok(diag) => diag.join("\n").into(),
                    Err(e) => format!("lint failed: {e}").into(),
                }
            };
            out
        }),

        "/fmt" => Some({
            let out: Handled = if !caller_is(ctx.principal, veles_core::acl::Cap::Shell) {
                denied(ctx.principal, "/fmt", veles_core::acl::Cap::Shell).into()
            } else if rest.trim().is_empty() {
                "usage: /fmt <file.tcl>  — format via tcl-lsp and write it back \
                 (a summary of what changed is shown; /fmt <file> check for a dry run)"
                    .into()
            } else {
                let mut parts = rest.split_whitespace();
                let path = parts.next().unwrap_or("");
                let dry = parts.next() == Some("check");
                match veles_core::lsp_check::fmt_file(path, 30) {
                    Ok(None) => format!("{path}: already formatted").into(),
                    Ok(Some(new)) => {
                        if dry {
                            format!(
                                "{path}: formatting available ({} → {} bytes) — /fmt {path} to write it",
                                std::fs::metadata(path)
                                    .map(|m| m.len())
                                    .unwrap_or(0),
                                new.len()
                            )
                            .into()
                        } else {
                            match std::fs::write(path, &new) {
                                Ok(()) => format!("{path}: formatted and written").into(),
                                Err(e) => format!("write failed: {e}").into(),
                            }
                        }
                    }
                    Err(e) => format!("fmt failed: {e}").into(),
                }
            };
            out
        }),

        // Start a background SHELL — the command form of `terminal
        // background=true`, for every surface and every user, not just
        // the model mid-turn. The taxonomy split (USER, 2026-09-17)
        // moved the subagent spawn this name used to carry to `/btw`;
        // "background" now means what the shell family has always meant
        // by it, and `/shells` lists, reads and kills what this starts.
        // Zone comes from the SESSION principal (Unset = the framework
        // decides), exactly like the tool form — a below-admin sender's
        // shell runs in their own box.
        "/background" | "/bg" => Some(if rest.is_empty() {
            "usage: /background <command>  — runs it as a background shell; /shells shows it"
                .into()
        } else {
            let id = veles_core::shells::start(
                rest,
                &agent.session_id,
                veles_core::user_envs::ZoneArg::Unset,
            );
            format!("background shell {id} started — /shells to watch, /shells kill {id} to stop")
        }
        .into()),

        // `/btw` — the question that deserves its own agent but not its
        // own conversation. Spawns a one-off subagent FORKED off this
        // session (the child inherits this transcript, so the question
        // can be "what did we just decide"), whose answer returns to
        // THIS session as a [BACKGROUND CHILD COMPLETE] injection at the
        // next turn boundary. The child is a real, titled, parent-linked
        // session — attachable from the list (↳ marks it) and named in
        // both the injection and the tasks overlay. It takes over the
        // subagent role the old `/background` carried under a name that
        // said "shell" to everyone who read it. The command, its tier
        // and its surfaces are settled so nothing has to move twice.
        "/btw" => Some(if rest.is_empty() {
            "usage: /btw <prompt>  — forks a one-off subagent off this session; its answer arrives back here"
                .into()
        } else {
            let id = veles_core::delegate::spawn_background_child(rest, &agent.session_id);
            format!(
                "subagent {id} started — /subagents to watch; its answer arrives here when it finishes"
            )
        }
        .into()),

        // Stop the turn running in this session.
        //
        // On the gateway this is a builtin instead (see
        // GATEWAY_OVERRIDES): getting here means taking the agent lock,
        // and there the running turn is holding it, so the cancel would
        // wait for the thing it is cancelling. In the REPL and the TUI
        // the dispatcher is not blocked, so this arm is reachable and
        // correct.
        "/cancel" | "/stop" => {
            let msg = if veles_core::agent::cancel_turn(&agent.session_id) {
                "cancelling the running turn…"
            } else {
                "nothing running in this session"
            };
            Some(msg.into())
        }

        // Move the running foreground command to the background — the
        // gentle sibling of /cancel (USER taxonomy, 2026-09-17:
        // "there should also be a way for the user to move a running
        // foreground shell into the background"). Arms a one-shot flag
        // the `terminal` tool's wait loop polls; on the next poll it
        // registers the live process as a shell row and answers the
        // tool call with the task id. Nothing is killed; the command
        // keeps running under the registry's cancel token from then on.
        //
        // Same lock story as /cancel: the flag arm does not touch the
        // agent lock, so it is reachable while a turn holds it — but
        // ONLY through the pre-lock paths (gateway fast lane, desktop
        // /api/detach, this dispatcher when the surface's command loop
        // is not blocked). request_detach refuses honestly when no
        // foreground terminal call is waiting right now.
        "/detach" => {
            let msg = if veles_core::agent::request_detach(&agent.session_id) {
                "detaching — the command keeps running as a background shell (its task id lands in the turn)"
            } else if veles_core::agent::turn_is_live(&agent.session_id) {
                "nothing to detach: the running turn is not inside a foreground terminal command right now"
            } else {
                "nothing running in this session"
            };
            Some(msg.into())
        }

        // Scheduled work. Existed in the CLI and the desktop; the REPL
        // and the TUI had no reference to cron at all, so two of the
        // surfaces you might sit in all day could neither see what was
        // scheduled nor tell that nothing was going to run it.
        "/crons" | "/cron" => Some(cron_command(agent, rest).into()),

        // `/session migrate|reclaim` — the wire legs are async (A2A
        // push/query), so this arm block_on's a fresh runtime, exactly
        // the /sandbox /quota shape; the gateway awaits the twin in
        // dispatch_async instead (GATEWAY_OVERRIDES). `owner` is a pure
        // store read and rides the same handler.
        "/session" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => rt
                .block_on(session_migration_command(
                    ctx,
                    rest,
                ))
                .into(),
            Err(e) => format!("runtime: {e}").into(),
        }),

        // Triggers: reactions to channel events without a turn. The
        // sync verbs are plain store work; `refresh`/`mask` talk to
        // slopdrop over HTTP and take the /sandbox shape (blocking
        // twin here, awaited in dispatch_async, explicit awaited arm
        // on the gateway before it ever reaches this dispatcher).
        "/triggers" | "/trigger" => {
            let first = rest.split_whitespace().next().unwrap_or_default();
            if first == "refresh" || first == "mask" {
                let cfg = agent.cfg.clone();
                return Some(match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt
                        .block_on(triggers_command_async(
                            &AclCtx::from_agent(agent, ctx.principal),
                            &cfg,
                            rest,
                        ))
                        .into(),
                    Err(e) => format!("runtime: {e}").into(),
                });
            }
            Some(triggers_command(&AclCtx::from_agent(agent, ctx.principal), rest).into())
        }

        // The UPSTREAM plan's quota. Async because it is HTTP to
        // Singapore, so this arm block_on's a fresh runtime and the
        // gateway awaits the twin in `dispatch_async` instead — the
        // `/slopdrop` and `/gen` shape. `/quota` is in
        // GATEWAY_OVERRIDES for that reason, and
        // `every_blocking_arm_has_an_awaited_twin` enforces the pairing.
        "/quota" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => rt
                .block_on(quota_command(
                    &AclCtx::from_agent(agent, ctx.principal),
                    &agent.cfg,
                    rest,
                    Some(agent.effective_model().0.as_str()),
                ))
                .into(),
            Err(e) => format!("runtime: {e}").into(),
        }),

        // `/models` — a provider's catalog, which discovery may fetch
        // over HTTP: blocking here, awaited in `dispatch_async` and
        // by the gateway (GATEWAY_OVERRIDES), like `/quota`.
        "/models" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => {
                let (provider, model, _, _) = agent.effective_model();
                rt.block_on(models_command(&agent.cfg, (&provider, &model), rest))
                    .into()
            }
            Err(e) => format!("runtime: {e}").into(),
        }),

        // The routes a NEW session starts on. Sync: `configedit::edit`
        // is plain filesystem work, so there is no runtime to build and
        // no awaited twin to keep in step.
        "/defaults" => Some(
            defaults_command(&AclCtx::from_agent(agent, ctx.principal), rest).into(),
        ),
        "/configsync" => Some(
            configsync_command(&AclCtx::from_agent(agent, ctx.principal), rest).into(),
        ),

        // ── moved in from the surfaces, RM4.3 ─────────────────────────

        // THE report-3 fix: /model used to be read-only on the TUI
        // ("use `veles model` in a terminal") and unreachable from IRC
        // and Telegram entirely. One writable implementation now.
        "/model" => Some(if rest.is_empty() {
            let (eff_provider, eff_model, eff_reasoning, _) = agent.effective_model();
            let shown = |s: String| if s.is_empty() { "(default)".to_string() } else { s };
            format!(
                "model:    {}\nprovider: {}\nreasoning: {}\naliases:  {}\n\
                 set with /model <name> — alias, model@provider, provider/model, \
                 or custom:<provider> · this session only\n\
                 model@provider is the unambiguous spelling: a model id may itself \
                 contain a slash (openrouter/free), so provider/model is tried only \
                 after no provider claims the whole name as a model.",
                shown(eff_model),
                shown(eff_provider),
                eff_reasoning.unwrap_or_else(|| "(default)".into()),
                if agent.cfg.model_aliases.is_empty() {
                    "(none)".into()
                } else {
                    agent.cfg.model_aliases.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )
        } else if !caller_is(ctx.principal, veles_core::acl::Cap::CmdSession) {
            // The door is status (bare /model reads); switching shapes
            // the session, so it keeps cmd-session.
            denied(ctx.principal, "/model <name>", veles_core::acl::Cap::CmdSession)
        } else {
            one_or_error(agent.set_model(rest))
        }
        .into()),

        // Reasoning effort + display, per session. The TUI's transcript
        // expansion toggle is a VIEW command and lives on the surface
        // (click a reasoning header, or /expand).
        "/reasoning" => Some({
            let mut out = String::new();
            if rest.is_empty() {
                let ov = agent.overrides.read_ok().clone();
                let lvl = ov
                    .reasoning
                    .clone()
                    .or_else(|| agent.cfg.agent.reasoning_effort.clone());
                out.push_str(&format!(
                    "reasoning: {} · display: {}",
                    lvl.unwrap_or_else(|| "default".into()),
                    if ov.show_reasoning {
                        "show"
                    } else {
                        "hide"
                    }
                ));
                if ctx.surface == Surface::Tui {
                    out.push_str(" · transcript blocks: click a ▶ header or /expand");
                }
            } else {
                match rest {
                    "show" | "hide" => {
                        agent.overrides.write_ok().show_reasoning = rest == "show";
                        out.push_str(&format!("(reasoning display: {rest})"));
                    }
                    lvl => {
                        agent.overrides.write_ok().reasoning = Some(lvl.to_string());
                        out.push_str(&format!("(reasoning effort: {lvl})"));
                    }
                }
            }
            out
        }
        .into()),

        // Whole-bot plan ledger for an owner surface; per-caller budgets
        // and spend for a gateway caller. An admin on a gateway can ask
        // about any subject with `/usage <subject>`.
        "/usage" => Some(match ctx.principal {
            None => usage_report(agent).into(),
            Some(_) => usage_quotas_command(&AclCtx::from_agent(agent, ctx.principal), rest).into(),
        }),

        "/context" => Some(format_context_report(&agent.context_report(None)).into()),

        // Session state + gateway lifecycle, merged: the REPL's session
        // report and the lifecycle view the TUI and the messaging
        // surfaces show, in one answer. (The gateway keeps its own
        // richer status_text() — declared in GATEWAY_OVERRIDES.)
        "/status" => Some(status_report(agent).into()),

        // Which build this is. The gateway answered it from its own
        // table and no other surface answered it at all — so `/version`
        // worked on IRC and Telegram (their inventory is `builtin_cap`,
        // which had a tier for it) and was an unknown command on the
        // REPL, the TUI and the desktop. It was in no taxonomy list and
        // no help table either, which is why nothing noticed for as long
        // as it took `every_gateway_builtin_is_declared` to exist.
        "/version" => Some(format!("veles {}", env!("CARGO_PKG_VERSION")).into()),

        // The UI language, as a command on every text surface (USER ask
        // 2026-09-20: "a dedicated button to swap"; per-surface, USER
        // 2026-09-25).
        //
        // On a gateway-served surface (IRC, Telegram, A2A, the desktop's
        // engine text) it sets THIS SESSION's language: the gateway is one
        // process answering many users, and the first version switched
        // that process — every user of every surface at once — while its
        // comment here claimed it only switched "the RUNNING process" and
        // "does NOT write config.toml" (the `primary`/`secondary` verbs
        // always did). On the TUI and REPL, single-user processes, it
        // still switches the process. Writing config.toml is `cmd-admin`,
        // checked inside, like `/quota`'s id-bearing verbs.
        //
        // The desktop's own chip and picker switch the WINDOW's language
        // (lib/langpick.tcl) and send it with every API request, so the
        // engine text a desktop shows follows the window unless the
        // session has its own.
        "/locale" | "/language" | "/lang" => Some(locale_command(ctx, rest).into()),

        // Where this session's tools run (PLAN-pool-round.md D1).
        "/tools" if rest.split_whitespace().next() == Some("home") => Some(
            tools_home_command(ctx, rest.trim_start().trim_start_matches("home").trim()).into(),
        ),

        "/tools" => Some({
            let resident = agent.tools.names();
            let mut deferred = agent.tools.deferred_names();
            deferred.extend(veles_core::deferred::deferred_names());
            deferred.sort();
            deferred.dedup();
            let mut out = format!("{} resident: {}", resident.len(), resident.join(", "));
            if !deferred.is_empty() {
                out.push_str(&format!(
                    "\n{} deferred (tool_search finds these, tool_call runs them): {}",
                    deferred.len(),
                    deferred.join(", ")
                ));
            }
            out
        }
        .into()),

        "/guards" => Some(guards_report(agent).into()),

        "/checkpoints" => Some({
            let items = agent
                .store
                .lock_ok()
                .checkpoint_list(Some(&agent.session_id), 20)
                .unwrap_or_default();
            format!(
                "{}\n(restore with /restore <id>)",
                veles_core::checkpoints::render_list(&items)
            )
        }
        .into()),

        "/restore" => Some(match rest.trim().parse::<i64>() {
            Ok(id) => match veles_core::checkpoints::restore(&agent.store, id) {
                Ok(msg) => msg,
                Err(e) => format!("error: {e}"),
            },
            Err(_) => "usage: /restore <checkpoint id>  (see /checkpoints)".into(),
        }
        .into()),

        "/todo" | "/todos" | "/tasks" => Some({
            let items = agent
                .store
                .lock_ok()
                .todos_list(&agent.session_id)
                .unwrap_or_default();
            veles_core::todo::render_list(&items)
        }
        .into()),

        "/plugins" => Some({
            let host = veles_core::pluginhost::global();
            match plugins_command(ctx.principal, rest) {
                Some(answer) => answer,
                None => {
                    let found = veles_core::plugins::discover(
                        &agent.cfg.plugins.enabled,
                        &agent.cfg.plugins.disabled,
                    )
                    .unwrap_or_default();
                    // A Tcl plugin's runtime state (R7): `unloaded`
                    // after `/plugins unload`, else loaded.
                    let mut tcl_state: std::collections::HashMap<String, &str> =
                        veles_core::tclrt::plugin_rows().into_iter().collect();
                    // The native tier's (R8): loaded | unloaded | refused.
                    for (name, state, _why) in veles_core::rustplugins::so_rows() {
                        tcl_state.insert(name, state);
                    }
                    // The script tier's (R9): loaded | error | unloaded |
                    // pending | no-host.
                    for row in veles_core::scripthost::rows() {
                        tcl_state.insert(row.name, row.state);
                    }
                    // "(no plugins)" above a list of running process
                    // plugins reads as a contradiction: name the tiers.
                    let mut out = if found.is_empty() {
                        "(no Tcl or Rust plugins)".to_string()
                    } else {
                        found
                            .iter()
                            .map(|p| {
                                let state = tcl_state
                                    .get(&p.manifest.name)
                                    .map(|s| format!(" {s}"))
                                    .unwrap_or_default();
                                format!("{:<24} kind={}{state}", p.manifest.name, p.manifest.kind)
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    if let Some(m) = veles_core::modulehost::global() {
                        out.push_str(&modules_listing(
                            m,
                            caller_is(ctx.principal, veles_core::acl::Cap::CmdAdmin),
                        ));
                    }
                    if let Some(h) = host {
                        out.push_str(&process_plugins_listing(
                            h,
                            caller_is(ctx.principal, veles_core::acl::Cap::CmdAdmin),
                        ));
                    }
                    out
                }
            }
        }
        .into()),

        "/steer" => Some(if rest.is_empty() {
            "usage: /steer <message>\n  injects after the next tool call — redirects the agent mid-turn".into()
        } else {
            agent
                .pending_steer
                .lock()
                .unwrap()
                .replace(rest.to_string());
            format!("steer queued: '{rest}' — will inject after next tool call")
        }
        .into()),

        "/branch" | "/fork" => Some({
            let store = agent.store.lock_ok();
            let sid = &agent.session_id;
            let new_id = crate::chat::new_session_id();
            // A branch IS a child session: parent link + the ONE copy
            // primitive (/btw mints through the same `copy_history` —
            // three hand-rolled load+append loops is how they drift).
            // Title inherited, "(branch of <parent title>)" appended so
            // the session list says where it came from.
            let title = store
                .title(sid)
                .ok()
                .flatten()
                .map(|t| format!("{t} (branch)"));
            if let Err(e) = store.create_child_session(&new_id, sid, title.as_deref()) {
                return Some(format!("error: {e}").into());
            }
            let n = store.copy_history(sid, &new_id).unwrap_or(0);
            format!(
                "(branched {n} messages → {new_id})\n(switch with: /resume {new_id})"
            )
        }
        .into()),

        "/export" => Some(export_session(agent, rest).into()),

        "/hooks" => Some(hooks_report().into()),

        // The shared message queue (R4.6): text queued while the turn
        // runs, from ANY surface. This IS the list — no private copies.
        "/queue" => Some(queue_command(agent, rest).into()),

        "/reload" => Some(
            "(nothing to reload — config, commands and skills are read \
             from disk per use)"
                .into(),
        ),

        "/diff" => Some({
            let mode = if rest.is_empty() { "session" } else { rest };
            let cmd = match mode {
                "staged" => "git diff --cached --stat",
                "all" => "git diff --stat",
                _ => "git diff --stat HEAD",
            };
            match veles_core::shell::std_command(cmd).output()
            {
                Ok(o) if o.status.success() => {
                    let s = String::from_utf8_lossy(&o.stdout).to_string();
                    if s.trim().is_empty() {
                        "(no changes)".into()
                    } else {
                        s
                    }
                }
                _ => "(not a git repo or git not available)".into(),
            }
        }
        .into()),

        "/image" | "/attach" => Some(if rest.is_empty() {
            "usage: /image <path|url>\n  attaches an image or text file to the next prompt".into()
        } else {
            match agent.attach(rest) {
                Ok(path) => format!(
                    "attached: {path}\n  pending: {} — send a prompt to include them",
                    agent.attachments_pending().len()
                ),
                Err(e) => format!("attach failed: {e}"),
            }
        }
        .into()),

        // The running-work families, split by kind (USER taxonomy,
        // 2026-09-17). `/tasks` is the TODO list above; these two are
        // the registry views, one kind each. Both route through the
        // single `tasks::kind_command` formatter — there is no second
        // listing, the same way `/shells` has no second shell reader.
        "/subagents" => Some(
            veles_core::tasks::kind_command(rest, &ctx.agent.session_id, veles_core::tasks::TaskKind::Subagent)
                .into(),
        ),

        "/monitors" => Some(
            veles_core::tasks::kind_command(rest, &ctx.agent.session_id, veles_core::tasks::TaskKind::Monitor)
                .into(),
        ),

        "/shells" => Some(veles_core::shells::command(rest, &ctx.agent.session_id).into()),

        "/profile" => Some({
            let active = veles_core::profiles::active_profile();
            let all = veles_core::profiles::list_profiles();
            let mut out = format!("active profile: {active}");
            for (name, is_active) in all {
                out.push_str(&format!(
                    "\n  {} {name}",
                    if is_active { "*" } else { " " }
                ));
            }
            out
        }
        .into()),

        "/skills" => Some({
            let set = veles_core::skills::SkillSet::load();
            if set.skills.is_empty() {
                "(no skills)".into()
            } else {
                let names: Vec<_> = set.skills.iter().map(|s| s.name.clone()).collect();
                format!("{} skills: {}", names.len(), names.join(", "))
            }
        }
        .into()),

        "/memory" => Some({
            let mem = veles_core::memory::MemoryStore::new();
            let m = mem.read(veles_core::memory::StoreKind::Memory);
            let u = mem.read(veles_core::memory::StoreKind::User);
            let files = mem.list().unwrap_or_default();
            let total: usize = files.iter().map(|f| f.chars).sum();
            format!(
                "MEMORY: {} chars\nUSER:   {} chars\nfiles:  {} ({total} chars in all) — \
                 `veles memory ls` lists them, `veles memory edit <path>` edits one",
                m.len(),
                u.len(),
                files.len()
            )
        }
        .into()),

        // Custom commands AND config aliases. Both are "words this
        // installation added", so they answer one question and belong in
        // one listing — an alias the user cannot find is a word with no
        // discoverable meaning, and `help_entries` is a static table that
        // cannot carry it.
        "/commands" => Some({
            let cmds = veles_core::commands::discover();
            let mut out = String::new();
            if cmds.is_empty() {
                out.push_str(&format!(
                    "no custom commands (drop .md files in ./.veles/commands/ or {}/)",
                    veles_core::config::config_dir().join("commands").display()
                ));
            } else {
                let rows: Vec<String> = cmds
                    .iter()
                    .map(|c| {
                        if c.description.is_empty() {
                            format!("/{}", c.name)
                        } else {
                            format!("/{} — {}", c.name, c.description)
                        }
                    })
                    .collect();
                out.push_str(&rows.join("\n"));
            }
            if let Some(l) = veles_core::aliases::listing(&agent.cfg) {
                out.push_str("\n\n");
                out.push_str(&l);
            }
            out
        }
        .into()),

        "/agents" => Some(veles_core::agents::listing().into()),

        // The compute pool's registry view (B3a): which nodes this host
        // knows, their liveness, and who arbitrates the singletons.
        // Same table on every surface — the merged cluster view arrives
        // with gossip, so this list GROWS as exchanges land.
        "/nodes" => Some({
            let store = agent.store.lock_ok();
            match store.nodes_list() {
                Err(e) => format!("error: {e}"),
                Ok(nodes) if nodes.is_empty() => {
                    "no nodes registered — start a gateway, or run `veles nodes probe`".into()
                }
                Ok(nodes) => {
                    let live_cfg = veles_core::config::load().ok();
                    let stale_after = live_cfg
                        .as_ref()
                        .map(|c| c.cluster.stale_after_secs)
                        .unwrap_or(600) as i64;
                    let me_id = veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
                        .ok()
                        .flatten()
                        .map(|i| i.id);
                    let mut out = String::new();
                    for n in &nodes {
                        let me_mark = if me_id.as_deref() == Some(n.id.as_str()) {
                            "*"
                        } else {
                            " "
                        };
                        let stale = if veles_core::node::is_stale(&n.heartbeat_at, stale_after) {
                            " stale"
                        } else {
                            ""
                        };
                        let gpu = n
                            .gpu
                            .as_deref()
                            .map(|g| {
                                serde_json::from_str::<serde_json::Value>(g)
                                    .ok()
                                    .and_then(|v| {
                                        v.get("name")
                                            .and_then(|x| x.as_str())
                                            .map(|s| s.to_string())
                                    })
                                    .map(|s| format!("  gpu={s}"))
                                    .unwrap_or_default()
                            })
                            .unwrap_or_default();
                        // B4: the placement metrics — load (normalized
                        // 0-100, '?' when unknown: unknown is NOT
                        // idle) and the lifetime session count.
                        let load = match n.load1 {
                            Some(l) => format!("  load={l}"),
                            None => "  load=?".to_string(),
                        };
                        let served = format!("  served={}", n.sessions_served);
                        let observed = n
                            .observed_addr
                            .as_deref()
                            .map(|a| format!("  seen={a}"))
                            .unwrap_or_default();
                        out.push_str(&format!(
                            "{me_mark} {} ({}) {} {}{gpu}{load}{served}{observed}{stale} — hb {}\n",
                            n.name, n.id, n.host, n.uptime_class, n.heartbeat_at
                        ));
                    }
                    // The singleton fence line, every time (the
                    // double-poller is the catastrophic mode; an unset
                    // arbiter must never render as fine).
                    let arb_cfg = live_cfg.map(|c| c.cluster).unwrap_or_default();
                    match arb_cfg
                        .arbiter
                        .as_deref()
                        .map(str::trim)
                        .filter(|a| !a.is_empty())
                    {
                        None => out.push_str(&veles_core::node::arbiter_unset_status(&arb_cfg)),
                        Some(_) => match veles_core::node::validate_arbiter(&arb_cfg, &store) {
                            Ok(full) => {
                                let who = nodes
                                    .iter()
                                    .find(|n| n.id == full)
                                    .map(|n| n.name.as_str())
                                    .unwrap_or("?");
                                out.push_str(&format!("arbiter: {who} ({full})"));
                            }
                            Err(e) => out.push_str(&format!("arbiter: {e:#}")),
                        },
                    }
                    out
                }
            }
        }
        .into()),

        "/failover" => Some(one_or_error(
            agent.set_failover_mode(if rest.is_empty() { None } else { Some(rest) }),
        )
        .into()),

        "/sessions" => Some({
            let store = agent.store.lock_ok();
            let current = &agent.session_id;
            store
                .list_sessions(15)
                .map(|sessions| {
                    sessions
                        .iter()
                        .map(|(id, title)| {
                            let preview = store.preview(id).unwrap_or_default();
                            let mark = if id == current { "*" } else { " " };
                            format!(
                                "{mark} {id}  {}",
                                if title.is_empty() { preview } else { title.clone() }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_else(|e| format!("error: {e}"))
        }
        .into()),

        "/title" => Some({
            let store = agent.store.lock_ok();
            if rest.is_empty() {
                let t = store
                    .title(&agent.session_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| "(unset)".into());
                format!("title: {t}")
            } else {
                match store.set_title(&agent.session_id, rest) {
                    Ok(()) => "(title set)".into(),
                    Err(e) => format!("error: {e}"),
                }
            }
        }
        .into()),

        "/secret" => Some({
            let sub = rest.split_whitespace().next().unwrap_or("");
            let name = rest
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .trim_start_matches('$')
                .to_string();
            match sub {
                "list" => {
                    let all = veles_core::secrets::listing();
                    if all.is_empty() {
                        "no secrets stored".into()
                    } else {
                        all.iter()
                            .map(|l| l.line())
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                }
                // Never takes the value: a secret typed into a chat
                // line is in that chat's history (and, on IRC or
                // Telegram, on somebody else's server) before anything
                // here could refuse it.
                "set" => format!(
                    "a secret is never typed into a chat: run `veles secret set {name}` in a \
                     terminal on the gateway's machine (it does not echo), or type \
                     `/secret set {name}` on the desktop, which opens a masked field"
                ),
                "unset" => {
                    let r = if veles_core::vault::local_values().contains_key(&name) {
                        veles_core::vault::unset_local(&name).map(|_| "the vault")
                    } else {
                        veles_core::secrets::unset(&name).map(|_| "secrets.env")
                    };
                    match r {
                        Ok(from) => format!("${name} removed from {from}"),
                        Err(e) => format!("error: {e}"),
                    }
                }
                _ => "usage: /secret list | set NAME | unset NAME".into(),
            }
        }
        .into()),

        // ── the transcript editors: the family that needed CommandCtx ──
        // A running turn owns the conversation. These refuse with an
        // honest message rather than racing it — the lock-order decision
        // this module's docs used to defer, made deliberately.

        "/clear" => Some(if ctx.turn_live {
            refuse_turn_live()
        } else {
            // Every live row — a count from the repaired `load_history`
            // came up short by the orphans it hides, and the newest rows
            // survived the clear (`Store::archive_all_live`).
            let count = agent
                .store
                .lock_ok()
                .archive_all_live(&agent.session_id)
                .unwrap_or(0);
            agent.forget_last_prompt();
            if let Some(h) = ctx.history.as_deref_mut() {
                h.clear();
            }
            Handled {
                text: format!(
                    "(context cleared — same session; {count} messages archived, still searchable)"
                ),
                reload: true,
            }
        }),

        "/undo" => Some(if ctx.turn_live {
            refuse_turn_live()
        } else {
            let n: u32 = rest.parse().unwrap_or(1).max(1);
            let dropped = agent.store.lock_ok().undo_turns(&agent.session_id, n);
            match dropped {
                Ok(dropped) => {
                    let h = agent
                        .store
                        .lock_ok()
                        .load_history(&agent.session_id)
                        .unwrap_or_default();
                    if let Some(hist) = ctx.history.as_deref_mut() {
                        *hist = h;
                    }
                    Handled {
                        text: format!("(undone: {dropped} message(s) dropped)"),
                        reload: true,
                    }
                }
                Err(e) => format!("error: {e}").into(),
            }
        }),

        // The REAL compaction — the same engine path the automatic
        // trigger and the desktop's /compress use. The gateway answers
        // this one itself (async context; GATEWAY_OVERRIDES).
        //
        // The body lives in `compress_command` so this arm and
        // `dispatch_async`'s share ONE implementation. Two copies of it
        // would drift, and the drift would be invisible: both would
        // still compile and only one of them would be right.
        "/compress" | "/compact" => Some(match tokio::runtime::Runtime::new() {
            Ok(rt) => rt.block_on(compress_command(ctx)),
            Err(e) => format!("runtime: {e}").into(),
        }),

        // Config-declared MEDIA aliases, the blocking twin of the arm in
        // `dispatch_async`. Needed because the REPL and the TUI call this
        // dispatcher and not the async one.
        //
        // The guard is what keeps this cheap: `alias_gen_kind` is a table
        // lookup on the already-loaded config, so an ordinary unknown
        // word still falls to `None` without ever building a runtime.
        //
        // `every_blocking_arm_has_an_awaited_twin` cannot police this
        // pair — it scrapes quoted slash-names out of match arm
        // heads, and an alias has no such literal anywhere in the source.
        // `both_dispatchers_serve_media_aliases` is the replacement.
        // A module's DIRECT command (R5): answered like a builtin. The
        // owner surfaces block on a fresh runtime here; the gateway and
        // the desktop await the twin in `dispatch_async`.
        _ if veles_core::commands::direct_command(head.trim_start_matches('/')).is_some() => {
            Some(match tokio::runtime::Runtime::new() {
                Ok(rt) => rt.block_on(direct_command_async(ctx, head, rest)).into(),
                Err(e) => format!("runtime: {e}").into(),
            })
        }
        _ if alias_gen_kind(&ctx.agent.cfg, head).is_some() => {
            match tokio::runtime::Runtime::new() {
                Ok(rt) => rt.block_on(media_alias(ctx, head, rest)),
                Err(e) => Some(format!("runtime: {e}").into()),
            }
        }

        _ => None,
    }
}

/// The `GenKind` a MEDIA alias names, or `None` for "not a media alias"
/// — a chat alias, a builtin, or an unknown word.
///
/// Split out of `media_alias` so both dispatchers can ask the question
/// without paying for an async context: in the sync dispatcher it is the
/// match guard that decides whether a runtime gets built at all.
fn alias_gen_kind(cfg: &veles_core::config::VelesConfig, head: &str) -> Option<GenKind> {
    use veles_core::aliases::AliasKind;
    match veles_core::aliases::resolve_command(cfg, head)?.kind {
        AliasKind::Chat => None,
        AliasKind::Image => Some(GenKind::Image),
        AliasKind::Video => Some(GenKind::Video),
        AliasKind::Audio => Some(GenKind::Audio),
    }
}

/// `/compress` — the real compaction, awaited.
///
/// Extracted from the dispatcher arm so the sync caller (which blocks
/// on it) and `dispatch_async` (which awaits it) run the SAME code.
async fn compress_command(ctx: &mut CommandCtx<'_>) -> Handled {
    if ctx.turn_live {
        return refuse_turn_live();
    }
    let agent = ctx.agent;
    let mut hist = agent
        .store
        .lock_ok()
        .load_history(&agent.session_id)
        .unwrap_or_default();
    let before = hist.len();
    match agent.compact_forced(&mut hist).await {
        Some(outcome) => {
            let after = hist.len();
            if let Some(h) = ctx.history.as_deref_mut() {
                *h = hist;
            }
            Handled {
                // The shared formatter: same words the automatic engine
                // note and the other three surfaces use, so a compaction
                // that barely helped cannot read as a success here and
                // elsewhere.
                text: format!("({})", outcome.report(before, after)),
                reload: true,
            }
        }
        // A summary that failed is not "nothing to compact": say why.
        None => match agent.take_compaction_failure() {
            Some(why) => format!("(compaction failed — the history is unchanged: {why})").into(),
            None => "(nothing to compact — history too short for a legal cut)".into(),
        },
    }
}

/// `dispatch`, for a caller that is ALREADY on a Tokio runtime.
///
/// `dispatch` is sync, so its handful of genuinely async arms each make
/// a fresh runtime and `block_on` it. From a sync caller that is
/// correct. From an async one it is fatal: `Runtime::new().block_on(..)`
/// on a worker thread panics with *"Cannot start a runtime from within
/// a runtime"*, and `lifecycle` turns a panic into an abort so the
/// supervisor restarts us. So a desktop user typing `/tcl` did not get
/// an error — they took the whole gateway down, core dump and all
/// (found 2026-09-12, while verifying the slopdrop cutover).
///
/// `command_run` already knew the hazard existed and carved out
/// `/sandbox` by hand. That fixed the one command somebody had hit and
/// left `/gen`, `/tcl`, `/eval`, `/tcladmin`, `/compress` and
/// `/compact` live — a hand-maintained list of "the async ones" being
/// the same shape of bug as a command taxonomy with audits and no
/// derivation. `every_blocking_arm_has_an_awaited_twin` now reads the
/// dispatcher's source and fails if an arm blocks without a twin here.
///
/// `None` means "not one of these": fall through to `dispatch`.
/// `/session migrate|reclaim|owner` (B6). Shares its verbs with the
/// CLI family; the ACL tier is CmdAdmin on every surface.
pub async fn session_migration_command(ctx: &mut CommandCtx<'_>, rest: &str) -> String {
    use veles_core::acl::Cap;
    if !caller_is(ctx.principal, Cap::CmdAdmin) {
        return denied(ctx.principal, "`/session`", Cap::CmdAdmin);
    }
    let mut words = rest.split_whitespace();
    let Some(verb) = words.next() else {
        return "usage: /session migrate <id> <node> [--force] | reclaim <id> | owner <id>"
            .to_string();
    };
    let home = veles_core::config::veles_home();
    let store = match veles_core::store::Store::open(&home.join("state.db")) {
        Ok(s) => s,
        Err(e) => return format!("cannot open the store: {e:#}"),
    };
    let me = match veles_core::node::NodeIdentity::load(&veles_core::config::config_dir())
        .ok()
        .flatten()
    {
        Some(m) => m,
        None => {
            return "this machine has no node identity (no node.toml) — start a gateway once, \
                    or run `veles nodes probe`"
                .to_string()
        }
    };
    match verb {
        "migrate" => {
            let Some(id) = words.next() else {
                return "usage: /session migrate <id> <node> [--force]".to_string();
            };
            let Some(node) = words.next() else {
                return "usage: /session migrate <id> <node> [--force]".to_string();
            };
            let force = words.any(|w| w == "--force");
            match veles_core::migration::migrate(&me, node, id, force).await {
                Ok(line) => line,
                Err(e) => format!("migration failed: {e:#}"),
            }
        }
        "reclaim" => {
            let Some(id) = words.next() else {
                return "usage: /session reclaim <id>".to_string();
            };
            match veles_core::migration::reclaim(&me, id).await {
                Ok(line) => line,
                Err(e) => format!("reclaim failed: {e:#}"),
            }
        }
        "owner" => {
            let Some(id) = words.next() else {
                return "usage: /session owner <id>".to_string();
            };
            let local = store.session_node_stamp(id).ok().flatten();
            // The tombstone/park outranks the local-row answer — same
            // discipline as the CLI's owner verb (one honest answer
            // per state, never "owned here" for a migrated session).
            if let Some(n) = local.as_deref() {
                if veles_core::migration::is_retired(Some(n)) {
                    let target = n
                        .trim_start_matches(veles_core::migration::RETIRED_PREFIX)
                        .split('@')
                        .next()
                        .unwrap_or("?");
                    return format!(
                        "session {id}: RETIRED here — migrated to node {target}; the local \
                         rows are the archived original (readable, not writable)"
                    );
                }
                if veles_core::migration::is_parked(Some(n)) {
                    return format!(
                        "session {id}: PARKED mid-migration ({n}) — `/session reclaim {id}` \
                         finishes or undoes it"
                    );
                }
            }
            match veles_core::storesync::session_owner(&store, id) {
                Ok(Some(owner)) => format!("session {id}: owned by node {owner}"),
                Ok(None) => format!("session {id}: unknown here and in every replica held"),
                Err(e) => format!("cannot answer: {e:#}"),
            }
        }
        other => format!("unknown /session verb '{other}' — migrate | reclaim | owner"),
    }
}

/// Run a module's direct command for this surface's caller (R5). The
/// capability floor is checked by core's `run_direct`, the same for
/// every surface.
async fn direct_command_async(ctx: &mut CommandCtx<'_>, head: &str, rest: &str) -> String {
    let bare = head.trim_start_matches('/');
    let Some(d) = veles_core::commands::direct_command(bare) else {
        return format!("unknown command /{bare}");
    };
    veles_core::commands::run_direct(
        bare,
        &d,
        veles_core::commands::DirectCall {
            args: rest.to_string(),
            principal: ctx.principal.cloned(),
            sender: None,
            session_id: ctx.agent.session_id.clone(),
        },
    )
    .await
}

/// The shared-command entry for a surface that must answer before its
/// session's turn lock. IRC uses this on the live connection: no Agent is
/// constructed and no provider turn is started. The caller's principal and
/// wire identity travel in `AclCtx`, so this is not an owner-surface bypass.
///
/// Keep this a dispatcher, not an invitation to grow a second command
/// implementation in the adapter. A command that needs an Agent stays in
/// the ordinary `dispatch_async` path.
pub async fn dispatch_async_as(ctx: &AclCtx<'_>, head: &str, rest: &str) -> Option<Handled> {
    match head {
        "/tcl" | "/eval" | "/tcladmin" => {
            Some(tcl_command_as(ctx, rest, head == "/tcladmin").await.into())
        }
        _ => None,
    }
}

pub async fn dispatch_async(ctx: &mut CommandCtx<'_>, head: &str, rest: &str) -> Option<Handled> {
    match head {
        // B6: session migration, on every surface through the shared
        // dispatcher (the backport discipline). Admin-only: it moves
        // history across machines. The migrate/reclaim legs are the
        // SAME core orchestrators the CLI runs — one code path, four
        // surfaces.
        "/session" => Some(session_migration_command(ctx, rest).await.into()),
        "/sandbox" | "/sandboxes" => {
            let rest = if head == "/sandboxes" && rest.trim().is_empty() {
                "list"
            } else {
                rest
            };
            Some(
                sandbox_command(&AclCtx::from_agent(ctx.agent, ctx.principal), rest)
                    .await
                    .into(),
            )
        }
        "/gen" | "/generate" => Some(
            gen_command(&AclCtx::from_agent(ctx.agent, ctx.principal), rest)
                .await
                .into(),
        ),
        _ if matches!(head, "/tcl" | "/eval" | "/tcladmin") => {
            dispatch_async_as(&AclCtx::from_agent(ctx.agent, ctx.principal), head, rest).await
        }
        // `/triggers` — the awaited twin of the dispatcher's blocking
        // arm. `triggers_command_async` internally answers the sync
        // verbs too, so this ONE arm covers the whole command.
        "/triggers" | "/trigger" => Some(
            triggers_command_async(
                &AclCtx::from_agent(ctx.agent, ctx.principal),
                &ctx.agent.cfg,
                rest,
            )
            .await
            .into(),
        ),
        "/quota" => Some(
            quota_command(
                &AclCtx::from_agent(ctx.agent, ctx.principal),
                &ctx.agent.cfg,
                rest,
                Some(ctx.agent.effective_model().0.as_str()),
            )
            .await
            .into(),
        ),
        "/models" => Some({
            let (provider, model, _, _) = ctx.agent.effective_model();
            models_command(&ctx.agent.cfg, (&provider, &model), rest)
                .await
                .into()
        }),
        "/compress" | "/compact" => Some(compress_command(ctx).await),
        // Config-declared aliases (`veles_core::aliases`). Last, so a builtin
        // always wins the word — `resolve` refuses a builtin name too, so
        // this is belt and braces on a rule that matters.
        //
        // ASYNC-NATIVE ON PURPOSE, and not merely as a style. The
        // scraping tests that police this module
        // (`every_blocking_arm_has_an_awaited_twin`,
        // `every_dispatcher_arm_is_declared_and_tiered`) work on string
        // LITERALS in the function body; an alias resolved through a
        // table is invisible to them. So an alias arm that span up its
        // own `Runtime::new().block_on` in `dispatch` would be checked by
        // nothing — and that exact mistake aborted the gateway on
        // 2026-09-12. Media aliases live only here; there is no blocking
        // twin to get wrong.
        _ => match veles_core::commands::direct_command(head.trim_start_matches('/')) {
            Some(_) => Some(direct_command_async(ctx, head, rest).await.into()),
            None => media_alias(ctx, head, rest).await,
        },
    }
}

/// A media command alias — `/qwen-image-3-pro a beautiful unicorn`.
///
/// Only the media kinds. A CHAT alias mints a session and rebinds it, and
/// a session is surface-owned state `CommandCtx` cannot reach — the same
/// reason `/new` is in `DUPLICATED_ACROSS_SURFACES` — so those are
/// handled where `/new` is, and this returns `None` for them rather than
/// half-doing it in the wrong place.
async fn media_alias(ctx: &mut CommandCtx<'_>, head: &str, rest: &str) -> Option<Handled> {
    let kind = alias_gen_kind(&ctx.agent.cfg, head)?;
    let a = veles_core::aliases::resolve_command(&ctx.agent.cfg, head)?;
    let actx = AclCtx::from_agent(ctx.agent, ctx.principal);
    // The floor, before anything is built. `gen_run` takes the per-kind
    // gates (video, audio) and the rate brake itself, so an alias cannot
    // route around a cap by not being spelled `/gen`.
    if let Some(d) = gen_denied(actx.principal) {
        return Some(d.into());
    }
    if rest.trim().is_empty() && kind != GenKind::Audio {
        return Some(
            format!(
                "nothing to make — `/{} <prompt>` ({}/{})",
                a.name, a.provider, a.model
            )
            .into(),
        );
    }
    let req = GenRequest {
        kind,
        provider: Some(a.provider.clone()),
        model: Some(a.model.clone()),
        voice: None,
        speed: None,
        list: false,
        prompt: rest.trim().to_string(),
    };
    Some(gen_run(&actx, req).await.into())
}

fn refuse_turn_live() -> Handled {
    "a turn is running — /cancel first, then edit the transcript".into()
}

/// `/usage` — this session, all sessions, then per provider with cache
/// rates. The provider table is the point: a row sitting at 0% next to
/// one at 96% is the fastest way to see which endpoint is not giving
/// you a prefix cache. The recent window comes FIRST for the same
/// reason — lifetime shares span code changes and read as broken when
/// the last twenty calls are fine.
fn usage_report(agent: &Agent) -> String {
    let store = agent.store.lock_ok();
    let mut out = String::new();
    for (label, sid) in [
        ("this session", Some(agent.session_id.as_str())),
        ("all sessions", None),
    ] {
        match store.usage_totals(sid) {
            Ok(u) => out.push_str(&format!(
                "{label}: {} calls · {} tok ({}+{}) · {} cached · {} uncached\n",
                u.calls,
                u.prompt_tokens + u.completion_tokens,
                u.prompt_tokens,
                u.completion_tokens,
                u.cached_tokens,
                u.prompt_tokens.saturating_sub(u.cached_tokens)
            )),
            Err(e) => out.push_str(&format!("{label}: error: {e}\n")),
        }
    }
    // THE PLAN SPLIT (USER 2026-09-20). A bare total across the
    // personal plan, the team plan, z.ai and Kimi is the conflation
    // being complained about: the pools are disjoint, so "1.9G tokens"
    // answers no question anybody asked. The split is the same one
    // `/api/usage` serves, from the same function.
    out.push_str("by plan (all sessions):\n");
    let rows = store.usage_by_provider_scoped(None).unwrap_or_default();
    let ledger = veles_core::tokenplan::ledger_by_plan(&rows);
    if ledger.is_empty() {
        out.push_str("  no usage recorded yet\n");
    }
    for r in &ledger {
        // The providers are named beside the plan: `ali-responses`
        // landing on the team pool is exactly the kind of thing a
        // reader should be able to check rather than trust.
        out.push_str(&format!(
            "  {:<9} {:>6} calls · {:>10} tok ({}+{}) · {}\n",
            r.label(),
            r.calls,
            r.total_tokens(),
            r.prompt_tokens,
            r.completion_tokens,
            r.providers.join(", ")
        ));
    }
    // …and this session's own split, when it has spent anything.
    let srows = store
        .usage_by_provider_scoped(Some(&agent.session_id))
        .unwrap_or_default();
    let sledger = veles_core::tokenplan::ledger_by_plan(&srows);
    if !sledger.is_empty() {
        out.push_str("by plan (this session):\n");
        for r in &sledger {
            out.push_str(&format!(
                "  {:<9} {:>6} calls · {:>10} tok\n",
                r.label(),
                r.calls,
                r.total_tokens()
            ));
        }
    }
    if let Ok(rows) = store.usage_by_provider() {
        for r in rows {
            let recent = match r.last_session_cache_rate() {
                Some(rate) => {
                    format!(
                        "{:.0}% cached last session ({} calls)",
                        rate * 100.0,
                        r.last_session_calls
                    )
                }
                None => "no cache reading".to_string(),
            };
            let lifetime = if r.prompt_tokens > 0 {
                format!(
                    "{:.0}% lifetime",
                    100.0 * r.cached_tokens as f64 / r.prompt_tokens as f64
                )
            } else {
                "—".to_string()
            };
            out.push_str(&format!(
                "  {}/{}: {} calls · {} tok · {recent} · {lifetime}\n",
                r.provider,
                r.model,
                r.calls,
                r.prompt_tokens + r.completion_tokens,
            ));
        }
    }
    out.trim_end().to_string()
}

/// `/context` — the unified context bill (R4.7). One renderer serves
/// every surface that prints text (TUI, gateway, telegram, irc all
/// dispatch the same command), so the units cannot drift between them
/// again. Chars and tokens ride side by side, ALWAYS labelled: the
/// fixed prefix per block, the compaction bar (capped — the claimed
/// window can no longer price compaction out of reach), the warn line,
/// and where history sits against both.
pub fn format_context_report(r: &serde_json::Value) -> String {
    let window = r["context_window_tokens"].as_u64().unwrap_or(0);
    let pct = r["compaction_trigger_percent"].as_f64().unwrap_or(0.0);
    let max_tok = r["compaction_max_context_tokens"].as_u64().unwrap_or(0);
    let warn_tok = r["compaction_warn_tokens"].as_u64().unwrap_or(0);
    let budget = r["compaction_budget_chars"].as_u64().unwrap_or(0) as usize;
    let history = r["history_chars"].as_u64().unwrap_or(0) as usize;
    let history_tok = history / 4;

    let mut out = format!(
        "model {} · window {} tokens\n",
        r["model"].as_str().unwrap_or("?"),
        window
    );
    out.push_str("fixed prefix (re-sent on EVERY round, before any history):\n");
    for b in r["prefix_blocks"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "  {:<30} {:>7} chars · ≈{:>6} tok\n",
            b["name"].as_str().unwrap_or("?"),
            b["chars"].as_u64().unwrap_or(0),
            b["tokens_est"].as_u64().unwrap_or(0),
        ));
    }
    out.push_str(&format!(
        "compaction bar: min({pct}% of window, {max_tok} tokens) = {budget} chars (≈{} tokens)\n",
        budget / 4
    ));
    out.push_str(&format!("warn line: {warn_tok} tokens\n"));
    // The CALLER's ceiling, when they have one. Not the same thing as
    // the bar above: the bar rewrites history, this stops the turn.
    // Shown here because "why did my turn not run" should be
    // answerable from the same command that explains everything else
    // about this session's context.
    let ceiling = r["context_ceiling_tokens"].as_u64().unwrap_or(0);
    if ceiling > 0 {
        out.push_str(&format!(
            "your ceiling: {ceiling} tokens ({}) — over it, a turn asks you to /compress or /new instead of running\n",
            r["principal"].as_str().unwrap_or("this caller")
        ));
    }
    out.push_str(&format!(
        "history: {history} chars (≈{history_tok} tokens) — {:.1}% of the bar",
        if budget > 0 {
            history as f64 / budget as f64 * 100.0
        } else {
            0.0
        }
    ));
    // Ground the warn line: where history sits relative to it is the
    // sentence's whole point — a number without the relation is noise.
    let unit = warn_tok as usize;
    out.push_str(if unit > 0 {
        if history_tok >= unit {
            " · OVER the warn line"
        } else {
            " · under the warn line"
        }
    } else {
        ""
    });
    out.push('\n');
    out.push_str(&format!(
        "session: {} calls · {} msgs · {} prompt + {} completion tokens\n",
        r["session_calls"].as_u64().unwrap_or(0),
        r["session_messages"].as_u64().unwrap_or(0),
        r["session_prompt_tokens"].as_u64().unwrap_or(0),
        r["session_completion_tokens"].as_u64().unwrap_or(0),
    ));
    out.push_str(&veles_core::agent::Agent::format_cache_line(r));
    out
}

/// `/locale` — the language surface.
///
/// Forms:
///   /locale                  list the shipped catalogs, current marked
///   /locale <id>             switch the RUNNING process
///   /locale set <id>         same, and say that it did not persist
///   /locale primary <id>     set the primary (writes config.toml via
///   /locale secondary <id>   the config editor, saying what it wrote)
///
/// The list marks the active id and the secondary, so a bare `/locale`
/// answers "what am I speaking, and what would swap do".
fn locale_command(ctx: &CommandCtx, rest: &str) -> String {
    use veles_core::acl::Cap;
    let rest = rest.trim();
    let have = veles_core::i18n::available();
    let sid = ctx.agent.session_id.clone();
    // Per-session everywhere the gateway process answers; per-process on
    // the single-user TUI and REPL.
    let per_session = !matches!(ctx.surface, Surface::Repl | Surface::Tui);
    let process = veles_core::i18n::current();
    let now_effective = veles_core::i18n::effective();

    if rest.is_empty() {
        if per_session {
            let mut out = veles_core::i18n::lookup(
                "this session: %{cur} · default: %{def}  (available: %{have})",
                &[
                    ("cur", now_effective),
                    ("def", process),
                    ("have", have.join(", ")),
                ],
            );
            out.push('\n');
            out.push_str(&veles_core::i18n::lookup(
                "  /locale <id> sets this session's language · /locale reset returns it to the default · /locale primary|secondary <id> writes config.toml (admin)",
                &[],
            ));
            return out;
        }
        // Both lines are user-visible, so both go through the catalog.
        // The `/locale <id>` spelling stays verbatim: a translated
        // command name is a command nobody can type.
        let mut out = veles_core::i18n::lookup(
            "locale: %{cur}  (available: %{have})",
            &[("cur", process), ("have", have.join(", "))],
        );
        out.push('\n');
        out.push_str(&veles_core::i18n::lookup(
            "  /locale <id> switches this process · add `primary`/`secondary` to write config.toml",
            &[],
        ));
        return out;
    }

    // Split off a leading verb; anything else is the id.
    let (verb, arg) = match rest.split_once(char::is_whitespace) {
        Some((v, a)) if matches!(v, "set" | "primary" | "secondary") => (v, a.trim()),
        _ if matches!(rest, "swap" | "reset" | "default") => (rest, ""),
        _ => ("set", rest),
    };

    // Apply a switch where this surface keeps its language, and answer
    // IN THE NEW LANGUAGE — the reply is the first thing rendered in it.
    let switch = |wanted: &str, from: &str| -> String {
        if per_session {
            if let Ok(store) = ctx.agent.store.lock() {
                if let Err(e) = store.session_locale_set(&sid, wanted) {
                    return veles_core::i18n::lookup(
                        "could not save this session's language: %{e}",
                        &[("e", format!("{e:#}"))],
                    );
                }
            }
            let _in_new = veles_core::i18n::ScopeGuard::enter(Some(wanted.to_string()));
            veles_core::i18n::lookup(
                "this session's language → %{now} (was %{from}) · other sessions are unchanged",
                &[("now", wanted.to_string()), ("from", from.to_string())],
            )
        } else {
            let now = veles_core::i18n::set_active(wanted);
            veles_core::i18n::lookup(
                "locale → %{now} (was %{from})",
                &[("now", now), ("from", from.to_string())],
            )
        }
    };

    match verb {
        "reset" | "default" => {
            if !per_session {
                return veles_core::i18n::lookup(
                    "usage: /locale <id> | /locale primary <id> | /locale secondary <id> | /locale swap",
                    &[],
                );
            }
            if let Ok(store) = ctx.agent.store.lock() {
                let _ = store.session_locale_clear(&sid);
            }
            let _in_default = veles_core::i18n::ScopeGuard::enter(Some(process.clone()));
            veles_core::i18n::lookup(
                "this session follows the default language again (%{def})",
                &[("def", process)],
            )
        }
        // `swap` toggles between the configured primary and secondary.
        "swap" => {
            let cfg = veles_core::config::load().ok();
            let (pri, sec) = cfg
                .as_ref()
                .map(|c| {
                    (
                        c.tui.locale.clone().unwrap_or_else(|| "en".into()),
                        c.tui.locale_secondary.clone(),
                    )
                })
                .unwrap_or_else(|| ("en".into(), None));
            let pri = veles_core::i18n::normalize_id(&pri);
            let sec = sec
                .map(|s| veles_core::i18n::normalize_id(&s))
                .filter(|s| *s != pri);
            let to = if now_effective == pri { sec } else { Some(pri) };
            match to {
                Some(id) => switch(&id, &now_effective),
                // Name the COMMAND that fixes it, not a config key: the
                // first version said "set tui.locale_secondary", which is
                // an instruction to go and edit a file by hand.
                None => veles_core::i18n::lookup(
                    "no swap target yet — set one with /locale secondary <id>",
                    &[],
                ),
            }
        }
        _ if arg.is_empty() => veles_core::i18n::lookup(
            "usage: /locale <id> | /locale primary <id> | /locale secondary <id> | /locale swap",
            &[],
        ),
        _ => {
            let wanted = veles_core::i18n::normalize_id(arg);
            if !have.iter().any(|l| l == &wanted) {
                return veles_core::i18n::lookup(
                    "no catalog for '%{arg}' — shipped: %{have}",
                    &[("arg", arg.to_string()), ("have", have.join(", "))],
                );
            }
            match verb {
                "primary" | "secondary" => {
                    // A config write, and on the gateway the primary is the
                    // default for EVERY user — so it is an admin act, not a
                    // session-shaping one.
                    if !caller_is(ctx.principal, Cap::CmdAdmin) {
                        return denied(ctx.principal, &format!("/locale {verb}"), Cap::CmdAdmin);
                    }
                    let key = if verb == "primary" {
                        "tui.locale"
                    } else {
                        "tui.locale_secondary"
                    };
                    // `configedit` is the same seam /api/config/set uses: the
                    // edit is made in place (comments and key order survive), a
                    // backup is written beside it, and an edit that would make
                    // the file unreadable is refused before anything lands.
                    let segs: Vec<&str> = key.split('.').collect();
                    let r = veles_core::configedit::edit(key, |doc| {
                        veles_core::configedit::set_value(
                            doc,
                            &segs,
                            toml_edit::Value::from(wanted.clone()),
                        )
                    });
                    match r {
                        Ok(_) => {
                            // The primary IS the process default, so it takes
                            // effect now for everything without a session
                            // language of its own.
                            let live = if verb == "primary" {
                                format!(" · default now: {}", veles_core::i18n::set_active(&wanted))
                            } else {
                                String::new()
                            };
                            veles_core::i18n::lookup(
                                "%{key} → %{wanted} (written to %{path}%{live})",
                                &[
                                    ("key", key.to_string()),
                                    ("wanted", wanted.clone()),
                                    (
                                        "path",
                                        veles_core::configedit::config_path().display().to_string(),
                                    ),
                                    ("live", live),
                                ],
                            )
                        }
                        Err(e) => veles_core::i18n::lookup(
                            "could not write config.toml: %{e}",
                            &[("e", format!("{e:#}"))],
                        ),
                    }
                }
                _ => {
                    let mut out = switch(&wanted, &now_effective);
                    if normalize_differs(arg, &wanted) {
                        out.push('\n');
                        out.push_str(&veles_core::i18n::lookup(
                            "  '%{arg}' resolved to '%{now}' — that is the catalog we ship for it",
                            &[("arg", arg.to_string()), ("now", wanted.clone())],
                        ));
                    }
                    if !per_session {
                        out.push('\n');
                        out.push_str(&veles_core::i18n::lookup(
                            "  this process only; /locale primary <id> writes config.toml",
                            &[],
                        ));
                    }
                    out
                }
            }
        }
    }
}

/// Whether a typed locale spelling resolved to a DIFFERENT catalog id —
/// worth one line saying so (`de_DE` → `de` is not; `pt` → `en` is).
fn normalize_differs(arg: &str, wanted: &str) -> bool {
    let typed = arg.trim().to_lowercase().replace('-', "_");
    typed != wanted && !typed.starts_with(&format!("{wanted}_"))
}

/// `/status` — the REPL's session report plus the gateway lifecycle
/// section the TUI and the messaging surfaces show. Merged rather than
/// chosen: they answer different questions and both belong.
fn status_report(agent: &Agent) -> String {
    let store = agent.store.lock_ok();
    let mut out = String::new();
    let n_msgs = store.user_turn_count(&agent.session_id).unwrap_or(0);
    let u = store
        .usage_totals(Some(&agent.session_id))
        .unwrap_or_default();
    out.push_str(&format!("session: {}\n", agent.session_id));
    // ONE snapshot, not two `read_ok()` alive at once: a recursive read
    // on `std::sync::RwLock` is allowed to deadlock when a writer is
    // already queued.
    let ov = agent.overrides.read_ok().clone();
    out.push_str(&format!(
        "model:   {} @ {}\n",
        ov.model
            .or_else(|| agent.cfg.model.default.clone())
            .unwrap_or_else(|| "?".into()),
        ov.provider
            .or_else(|| agent.cfg.model.provider.clone())
            .unwrap_or_else(|| "?".into())
    ));
    out.push_str(&format!(
        "turns:   {n_msgs} user · {} LLM calls · {} tok\n",
        u.calls,
        u.prompt_tokens + u.completion_tokens
    ));
    out.push_str(&format!(
        "home:    {}\n",
        veles_core::config::veles_home().display()
    ));
    if veles_core::plan::mode_for(&store, &agent.session_id) == veles_core::plan::Mode::Plan {
        out.push_str("mode:    PLAN — reads and read-only shell only (/plan off)\n");
    }
    // Gateway lifecycle, best-effort: no gateway → say so.
    match veles_core::lifecycle::read_pid() {
        Some(pid) if veles_core::lifecycle::is_running(pid) => {
            out.push_str(&format!("gateway: running (pid {pid})\n"));
            if let Some(up) = veles_core::lifecycle::uptime_from_records() {
                out.push_str(&format!("uptime:  {up}s\n"));
            }
        }
        Some(pid) => out.push_str(&format!("gateway: stale pidfile ({pid})\n")),
        None => out.push_str("gateway: not running\n"),
    }
    for r in veles_core::lifecycle::recent_records(4) {
        out.push_str(&format!("  {r}\n"));
    }
    out.trim_end().to_string()
}

/// `/guards` — the harness snapshot, with the edited-paths tail the
/// REPL kept and the TUI dropped.
fn guards_report(agent: &Agent) -> String {
    let snap = agent.guards_snapshot();
    let it = &snap["iterations"];
    let gr = &snap["guardrails"];
    let v = &snap["verification"];
    let yn = |b: bool| if b { "yes" } else { "no" };
    let mut out = format!(
        "harness guards (config: [agent.guards] in config.toml)\n  iterations:    {} / {} used\n",
        it["used"], it["cap"]
    );
    out.push_str(&format!(
        "  repeat guard:  warn@{} halt@{} identical calls\n",
        gr["warn_at"], gr["halt_at"]
    ));
    out.push_str(&format!(
        "  verification:  {} · edits={} · fresh-evidence={} · nudges={}/{}\n",
        if v["enabled"].as_bool().unwrap_or(false) {
            "on"
        } else {
            "off"
        },
        yn(v["edits_seen"].as_bool().unwrap_or(false)),
        yn(v["fresh_evidence"].as_bool().unwrap_or(false)),
        v["nudges_this_turn"],
        agent.cfg.agent.guards.verification_max_attempts
    ));
    if let Some(paths) = v["edited_paths"].as_array() {
        if !paths.is_empty() {
            let shown: Vec<&str> = paths.iter().take(8).filter_map(|p| p.as_str()).collect();
            out.push_str(&format!("  edited paths:  {}\n", shown.join(", ")));
        }
    }
    out.trim_end().to_string()
}

/// `/hooks` — the taxonomy as a report: which hooks exist, which the
/// engine actually fires (vs. reserved for hermes plugins), and whether
/// anything is listening. The desktop renders this as a pane; every
/// other surface gets this text. `has_handler` is conservative — an
/// unknown registry answers yes — so a blank here means "no bus
/// published", which is itself the honest reading.
fn hooks_report() -> String {
    let rust = veles_core::hooks::rust_hook_names();
    let tcl = veles_core::hooks::default_bus()
        .map(|b| b.has_tcl())
        .unwrap_or(false);
    let mut out = format!(
        "hooks: {} known · {} fired by the engine · {} rust plugin handlers · tcl {}\n",
        veles_core::hooks::KNOWN_HOOKS.len(),
        veles_core::hooks::FIRED_HOOKS.len(),
        rust.len(),
        if tcl { "yes" } else { "no" },
    );
    for h in veles_core::hooks::KNOWN_HOOKS {
        let fired = if veles_core::hooks::FIRED_HOOKS.contains(h) {
            "fires"
        } else {
            "reserved"
        };
        let listens = veles_core::hooks::has_handler(h);
        out.push_str(&format!(
            "  {h:<26} {fired:<9} {}\n",
            if listens { "handler" } else { "—" }
        ));
    }
    out.push_str("(reserved = accepted for hermes plugins, nothing fires it)");
    out
}

/// `/queue` — the shared message queue (R4.6). One implementation,
/// dispatched everywhere: LIST shows pending and running alike, with
/// the state column explaining which is which; `cancel <seq>` removes
/// a pending row; `edit <seq> <text…>` rewrites one. Everything else
/// the gate would need is a turn away — this surface's job here is
/// visibility and veto, not conversation.
pub(crate) fn queue_command(agent: &veles_core::agent::Agent, rest: &str) -> String {
    let store = agent.store.lock_ok();
    let sid = &agent.session_id;
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("");
    match sub {
        "" | "list" => {
            let items = store.queue_items(sid).unwrap_or_default();
            if items.is_empty() {
                return "(queue empty)".into();
            }
            let mut out = String::from("session queue:\n");
            for it in &items {
                out.push_str(&format!(
                    "  #{:<3} [{}] {}\n",
                    it.seq,
                    it.state,
                    first_line(&it.body)
                ));
            }
            out.push_str("/queue cancel <seq> · /queue edit <seq> <text…>");
            if items.iter().any(|i| i.state == "held") {
                out.push_str(
                    "\nheld: these came with the session from another node and run only when \
                     released — /queue run <seq> · /queue run all",
                );
            }
            out
        }
        // D2: queued messages that arrived with a session from another
        // node wait to be released here (store.rs `queue_run_held`).
        "run" | "release" => {
            let which = match parts.next() {
                Some("all") | None => None,
                Some(n) => match n.parse::<i64>() {
                    Ok(seq) => Some(seq),
                    Err(_) => return "usage: /queue run <seq> · /queue run all".into(),
                },
            };
            match store.queue_run_held(sid, which) {
                Ok(0) => "no held message to release".into(),
                Ok(n) => format!(
                    "{n} held message(s) released — they run when this session's next turn \
                     ends (send anything to start one)"
                ),
                Err(e) => format!("error: {e}"),
            }
        }
        "cancel" | "rm" => {
            let Some(seq) = parts.next().and_then(|s| s.parse::<i64>().ok()) else {
                return "usage: /queue cancel <seq>".into();
            };
            match store.queue_delete(sid, seq) {
                Ok(true) => format!("queue #{seq} cancelled"),
                Ok(false) => format!("no pending message #{seq}"),
                Err(e) => format!("error: {e}"),
            }
        }
        "edit" => {
            let Some(seq) = parts.next().and_then(|s| s.parse::<i64>().ok()) else {
                return "usage: /queue edit <seq> <text…>".into();
            };
            let body: Vec<&str> = parts.collect();
            if body.is_empty() {
                return "usage: /queue edit <seq> <text…>".into();
            }
            match store.queue_patch(sid, seq, &body.join(" ")) {
                Ok(true) => format!("queue #{seq} rewritten"),
                Ok(false) => format!("no pending message #{seq}"),
                Err(e) => format!("error: {e}"),
            }
        }
        _ => "usage: /queue [list] · cancel <seq> · edit <seq> <text…> · run <seq>|all".into(),
    }
}

/// `/tools home` (PLAN-pool-round.md D1): where this session's tool calls
/// run. Bare, it lists the rules that apply — the session's own, then
/// `[tools.home]`. `<glob> <node>` sends matching tools to a node;
/// `<glob> local` keeps them here over a config rule; `<glob> -` drops
/// the session's rule. Setting one needs cmd-session, like `/model`: it
/// shapes where the session's work happens. What the other node runs
/// without a ticket is remotetool.rs's rule, whoever set the home.
fn tools_home_command(ctx: &CommandCtx, rest: &str) -> String {
    let agent = ctx.agent;
    let sid = &agent.session_id;
    let mut parts = rest.split_whitespace();
    let usage = "usage: /tools home · /tools home <glob> <node|local> · /tools home <glob> -";
    match (parts.next(), parts.next(), parts.next()) {
        (None, _, _) => {
            let mine = agent.store.lock_ok().tool_homes(sid).unwrap_or_default();
            let cfg = veles_core::config::live();
            if mine.is_empty() && cfg.tools.home.is_empty() {
                return format!(
                    "every tool runs on this node (no /tools home or [tools.home] rule)\n{usage}"
                );
            }
            let mut out = String::from(
                "tool homes (the most specific glob wins; this session's rules first):\n",
            );
            for (glob, node) in &mine {
                out.push_str(&format!("  {glob} → {node}   (this session)\n"));
            }
            for (glob, node) in &cfg.tools.home {
                out.push_str(&format!("  {glob} → {node}   ([tools.home])\n"));
            }
            out.push_str(usage);
            out
        }
        (Some(glob), Some(node), None) => {
            if !caller_is(ctx.principal, veles_core::acl::Cap::CmdSession) {
                return "[acl] denied: /tools home <glob> <node> changes where this session's \
                        tools run, which needs cmd-session"
                    .into();
            }
            let store = agent.store.lock_ok();
            if matches!(node, "-" | "clear" | "off") {
                return match store.tool_home_clear(sid, glob) {
                    Ok(true) => format!("{glob}: this session's rule dropped"),
                    Ok(false) => format!("{glob}: this session had no rule for it"),
                    Err(e) => format!("error: {e}"),
                };
            }
            let known = veles_core::remotetool::is_here(node)
                || matches!(
                    store.node_resolve(node),
                    Ok(veles_core::store::NodeMatch::One(_))
                )
                || store
                    .nodes_list()
                    .map(|l| l.iter().any(|n| n.name == node))
                    .unwrap_or(false);
            if !known {
                return format!(
                    "no node '{node}' in the registry — `veles nodes list` names the pool \
                     (`local` keeps tools here)"
                );
            }
            match store.tool_home_set(sid, glob, node) {
                Ok(()) if veles_core::remotetool::is_here(node) => {
                    format!("{glob}: runs on this node for this session")
                }
                Ok(()) => format!(
                    "{glob}: runs on node {node} for this session — at the user level there \
                     unless a ticket raises a call"
                ),
                Err(e) => format!("error: {e}"),
            }
        }
        _ => usage.into(),
    }
}

/// First line, truncated — queue rows are listed one per line, and a
/// pasted wall of text must not own the report.
fn first_line(body: &str) -> String {
    let line = body.lines().next().unwrap_or(body);
    let line = line.trim();
    let mut chars = line.chars();
    let head: String = chars.by_ref().take(48).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head.to_string()
    }
}

/// `/export [md|json]` — writes the file (same box as the session
/// store) and says where; the session_export tool does the same job
/// when a surface prefers a download over a path.
fn export_session(agent: &Agent, rest: &str) -> String {
    let store = agent.store.lock_ok();
    let sid = agent.session_id.clone();
    let msgs = store.load_history(&sid).unwrap_or_default();
    let title = store
        .title(&sid)
        .ok()
        .flatten()
        .unwrap_or_else(|| sid.clone());
    let fmt = if rest == "json" { "json" } else { "markdown" };
    let path = std::env::temp_dir()
        .join(format!(
            "veles_export_{sid}.{}",
            if fmt == "json" { "json" } else { "md" }
        ))
        .to_string_lossy()
        .into_owned();
    let output = if fmt == "json" {
        let entries: Vec<serde_json::Value> = msgs
            .iter()
            .map(|m| {
                serde_json::json!({"role": m.role, "content": m.content, "reasoning": m.reasoning})
            })
            .collect();
        serde_json::json!({"session_id": sid, "title": title, "messages": entries}).to_string()
    } else {
        let mut out = format!(
            "# {title}\n\n> Session `{sid}` · {} messages\n\n---\n\n",
            msgs.len()
        );
        for msg in &msgs {
            match msg.role.as_str() {
                "user" => out.push_str(&format!("## User\n\n{}\n\n", msg.content)),
                "assistant" => {
                    if let Some(r) = &msg.reasoning {
                        if !r.is_empty() {
                            out.push_str("<details><summary>Reasoning</summary>\n\n");
                            out.push_str(r);
                            out.push_str("\n\n</details>\n\n");
                        }
                    }
                    out.push_str(&format!("## Assistant\n\n{}\n\n", msg.content));
                }
                _ => {}
            }
        }
        out
    };
    match std::fs::write(&path, output) {
        Ok(()) => format!("(exported {} messages → {path})", msgs.len()),
        Err(e) => format!("export failed: {e}"),
    }
}

/// `/crons` — list, add, run, pause, resume, remove, history. Alias: /cron.
///
/// `run` does NOT execute the job here. The CLI can afford
/// `rt.block_on(run_job_manual(..))` because it owns its runtime; this
/// runs inside the gateway's, where building a second one panics. So
/// `run` schedules the job for now and lets the real scheduler take it,
/// which is also the more honest path: one execution route, and the
/// reply says plainly whether anything is alive to pick it up.
fn cron_command(agent: &Agent, rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("");
    let store = &agent.store;

    match sub {
        "" | "list" => veles_core::cron::listing(store),

        "add" => cron_add(agent, rest.trim_start_matches("add").trim()),

        "run" => {
            if arg.is_empty() {
                return "usage: /crons run <id>".into();
            }
            let s = store.lock_ok();
            match s.cron_get(arg) {
                Ok(Some(_)) => {
                    let now = chrono::Utc::now().to_rfc3339();
                    if let Err(e) = s.cron_set_next(arg, Some(&now)) {
                        return format!("error: {e}");
                    }
                    match veles_core::lifecycle::read_pid()
                        .filter(|p| veles_core::lifecycle::is_running(*p))
                    {
                        Some(_) => {
                            format!("{arg} queued — the scheduler picks it up within a tick")
                        }
                        None => format!(
                            "{arg} queued, but NO scheduler is running: it fires when a \
                             gateway starts, not now"
                        ),
                    }
                }
                Ok(None) => format!("no job with id '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        "pause" | "resume" => {
            if arg.is_empty() {
                return format!("usage: /crons {sub} <id>");
            }
            let on = sub == "resume";
            match store.lock_ok().cron_set_enabled(arg, on) {
                Ok(true) => format!("job {arg} {}d", sub),
                Ok(false) => format!("no job with id '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        "remove" | "rm" | "delete" => {
            if arg.is_empty() {
                return "usage: /crons remove <id>".into();
            }
            match store.lock_ok().cron_delete(arg) {
                Ok(true) => format!("job {arg} removed"),
                Ok(false) => format!("no job with id '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        "history" => {
            if arg.is_empty() {
                return "usage: /crons history <id>".into();
            }
            match store.lock_ok().cron_run_history(arg, 10) {
                Ok(runs) if runs.is_empty() => format!("no runs recorded for {arg}"),
                Ok(runs) => {
                    let mut out = format!("last {} run(s) of {arg}:\n", runs.len());
                    for r in runs {
                        out.push_str(&format!(
                            "  {} {} {}\n",
                            r.started_at,
                            r.status,
                            r.detail.as_deref().unwrap_or("")
                        ));
                    }
                    out.trim_end().to_string()
                }
                Err(e) => format!("error: {e}"),
            }
        }

        other => format!(
            "unknown: /crons {other}\n\
             usage: /crons · /crons add \"<schedule>\" [session=<id>] <prompt> · \
             /crons run|pause|resume|remove|history <id>"
        ),
    }
}

/// `/crons add "<schedule>" [session=<id>] <prompt>`.
///
/// The schedule is quoted because "every 2 hours" has spaces and the
/// prompt follows it; `normalize_schedule` is what decides whether it is
/// a schedule at all, so an unparseable one is refused here rather than
/// stored and discovered at fire time. The optional `session=` token
/// (Stage 5) makes the job a WAKEUP: it resumes that session with its
/// history instead of running in a fresh one.
fn cron_add(agent: &Agent, rest: &str) -> String {
    let (raw, tail) = match rest.strip_prefix('"').and_then(|r| r.split_once('"')) {
        Some((s, p)) => (s.trim(), p.trim()),
        None => {
            return "usage: /crons add \"<schedule>\" [session=<id>] <prompt>  (quote the schedule)"
                .into()
        }
    };
    // Optional `session=<ref>` token turns the job into a WAKEUP for
    // that session (Stage 5) instead of a fresh-session run. It leads
    // the tail so the prompt can still start with any word.
    let (session, prompt) = match tail.split_once(char::is_whitespace) {
        Some((head, rem)) if head.starts_with("session=") => (
            Some(head.strip_prefix("session=").unwrap().trim()),
            rem.trim(),
        ),
        _ => (None, tail),
    };
    if prompt.is_empty() {
        return "a job needs a prompt: /crons add \"<schedule>\" [session=<id>] <prompt>".into();
    }
    let s = agent.store.lock_ok();
    let summary = match veles_core::cron::add_local_job(&s, raw, prompt, session) {
        Ok(m) => m,
        Err(e) => return format!("error: {e:#}"),
    };
    format!(
        "{summary}\n{}",
        match veles_core::lifecycle::read_pid().filter(|p| veles_core::lifecycle::is_running(*p)) {
            Some(pid) => format!("the scheduler is running (gateway pid {pid})"),
            None =>
                "NOTE: no scheduler is running, so this will not fire until a gateway is up".into(),
        }
    )
}

/// `""` → `None`, so every "no argument means report, argument means
/// set" command can hand its rest straight to the agent.
fn opt(rest: &str) -> Option<&str> {
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn one_or_error(r: anyhow::Result<String>) -> String {
    match r {
        Ok(m) => m,
        Err(e) => format!("error: {e}"),
    }
}

// ── /triggers (plan item 1) ────────────────────────────────────────

/// `/triggers` — the control surface for reactions to channel events
/// that run without a turn. AclCtx-shaped (like /acl and /limits):
/// listing and editing needs no LLM session, only the store.
///
/// The two verbs that talk to slopdrop over HTTP (`refresh`, `mask`)
/// have their async twin in `triggers_command_async`; this sync body
/// answers everything else.
pub(crate) fn triggers_command(ctx: &AclCtx, rest: &str) -> String {
    let mut parts = rest.split_whitespace();
    let sub = parts.next().unwrap_or_default();
    let arg = parts.next().unwrap_or_default();
    let store = ctx.store;

    match sub {
        "" | "list" => veles_core::triggers::listing(store),

        "show" => {
            if arg.is_empty() {
                return "usage: /triggers show <name>".into();
            }
            match store.lock_ok().trigger_get(arg) {
                Ok(Some(t)) => format!(
                    "{}: {} {} {} {} match={} action={}{}\ncooldown={}s cap={}h fires={}",
                    t.name,
                    t.platform.as_deref().unwrap_or("*"),
                    t.network.as_deref().unwrap_or("*"),
                    t.channel.as_deref().unwrap_or("*"),
                    t.event,
                    t.match_kind,
                    t.action,
                    t.pattern
                        .as_deref()
                        .map(|p| format!(" pattern={p}"))
                        .unwrap_or_default(),
                    t.cooldown_secs,
                    t.per_hour_cap,
                    t.fire_count,
                ),
                Ok(None) => format!("no trigger named '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        "add" => triggers_add(store, rest.trim_start_matches("add").trim()),

        // The RAW remainder, not `parts.join(" ")`. The quote characters
        // survive a split/join round trip, so this is narrower than it
        // first looks — what it destroys is INTERIOR WHITESPACE: every
        // run inside a quoted value collapses to one space, and tabs
        // become spaces. A value is a value, and a surface that quietly
        // reformats one is the same class of defect as one that halves
        // it. `splitn(3)` keeps everything after the name verbatim.
        "edit" => {
            let mut it = rest.splitn(3, char::is_whitespace);
            let _ = it.next(); // "edit"
            let name = it.next().unwrap_or_default();
            triggers_edit(
                store,
                name,
                it.next().unwrap_or_default().trim().to_string(),
            )
        }

        "enable" | "disable" => {
            if arg.is_empty() {
                return format!("usage: /triggers {sub} <name>");
            }
            match store.lock_ok().trigger_set_enabled(arg, sub == "enable") {
                Ok(true) => format!("{arg} {}d", sub),
                Ok(false) => format!("no trigger named '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        "rm" | "remove" | "delete" => {
            if arg.is_empty() {
                return "usage: /triggers rm <name>".into();
            }
            match store.lock_ok().trigger_delete(arg) {
                Ok(true) => format!("{arg} removed"),
                Ok(false) => format!("no trigger named '{arg}'"),
                Err(e) => format!("error: {e}"),
            }
        }

        // Dry run: does the text match, and what WOULD fire — without
        // calling the bridge or the vision model.
        "test" => {
            let mut it = rest.splitn(3, char::is_whitespace);
            let _ = it.next(); // "test"
            let name = it.next().unwrap_or_default();
            let text = it.next().unwrap_or_default().trim();
            if name.is_empty() || text.is_empty() {
                return "usage: /triggers test <name> <text>".into();
            }
            let Ok(Some(row)) = store.lock_ok().trigger_get(name) else {
                return format!("no trigger named '{name}'");
            };
            let t = row.clone().into_trigger();
            let matched = t.matches_text(text);
            format!(
                "{name}: {}match{}\nwould {}",
                if matched { "" } else { "NO " },
                if t.enabled {
                    ""
                } else {
                    " (but the row is disabled)"
                },
                match t.action {
                    veles_core::triggers::ActionKind::Slopdrop =>
                        "forward to slopdrop and relay the reply".into(),
                    veles_core::triggers::ActionKind::Vision => format!(
                        "describe {} through the [vision] route",
                        veles_core::triggers::extract_urls(text)
                            .first()
                            .cloned()
                            .unwrap_or_default()
                    ),
                    veles_core::triggers::ActionKind::Bluesky => {
                        let links = veles_core::bskylink::links_in(text);
                        if links.is_empty() {
                            "find no bluesky link to preview".into()
                        } else {
                            format!(
                                "preview {} as the bot{}",
                                links
                                    .iter()
                                    .map(|l| l.url.as_str())
                                    .collect::<Vec<_>>()
                                    .join(" and "),
                                if veles_core::bskylink::bot_available() {
                                    ""
                                } else {
                                    " — but no bot is here, so slopdrop's resolver answers"
                                }
                            )
                        }
                    }
                    veles_core::triggers::ActionKind::Prompt => "start a turn".into(),
                    veles_core::triggers::ActionKind::Command => "run a command".into(),
                },
            )
        }

        // `/triggers log [name] [n]`. The two optional tokens are told
        // apart by SHAPE, not by position: a bare number is the limit
        // wherever it lands. Reading position instead made
        // `/triggers log 20` filter for a trigger called "20" and then
        // report the empty result as "no fires recorded" — the same
        // sentence a healthy trigger that has never fired produces, so
        // the typo pointed at the trigger instead of at the spelling.
        "log" => {
            let mut name = if arg.is_empty() {
                None
            } else {
                Some(arg.to_string())
            };
            let mut limit: u32 = 10;
            if let Some(n) = name.as_deref().and_then(|a| a.parse::<u32>().ok()) {
                limit = n;
                name = None;
            } else if let Some(p) = parts.next() {
                match p.parse::<u32>() {
                    Ok(n) => limit = n,
                    Err(_) => return format!("usage: /triggers log [name] [n] — '{p}' is neither"),
                }
            }
            let guard = store.lock_ok();
            // One lock for the whole arm: the existence check and the
            // history read share it, so neither re-locks.
            if let Some(n) = name.as_deref() {
                match guard.trigger_get(n) {
                    Ok(Some(_)) => {}
                    // An unknown name is its own answer and must not
                    // borrow the healthy-empty sentence.
                    Ok(None) => return format!("no trigger named '{n}'"),
                    Err(e) => return format!("error: {e}"),
                }
            }
            match guard.trigger_fire_history(name.as_deref(), limit) {
                Ok(rows) if rows.is_empty() => match name.as_deref() {
                    Some(n) => format!("no fires recorded for {n}"),
                    None => "no fires recorded".to_string(),
                },
                Ok(rows) => {
                    let mut out = format!("last {} fire(s):\n", rows.len());
                    for f in rows {
                        // The reason is the point of a refusal: the nine
                        // mask rows that diagnosed the crate-boundary bug
                        // said only "refused" here while the REST history
                        // said "mask: event not wanted by this channel".
                        out.push_str(&format!(
                            "  {} {} {} {}{}\n",
                            f.at,
                            f.name,
                            f.channel,
                            f.status,
                            f.detail
                                .as_deref()
                                .map(|d| format!(" — {d}"))
                                .unwrap_or_default()
                        ));
                    }
                    out.trim_end().to_string()
                }
                Err(e) => format!("error: {e}"),
            }
        }

        other => format!(
            "unknown: /triggers {other}\n\
             usage: /triggers · show|enable|disable|rm|test <name> · \
             add|edit <name> k=v… · log [name] [n] · \
             mask <net> <chan> <EVENTS|auto> · refresh"
        ),
    }
}

/// `/triggers add <name> k=v>…` — k ∈ network channel event match
/// pattern action arg cooldown cap min-role.
fn triggers_add(
    store: &std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
    rest: &str,
) -> String {
    // Quote-aware, because a multi-word value is the normal case for
    // `arg=` — a vision row carries an English question — and
    // `split_whitespace` made one untypeable from any surface.
    let toks = match veles_core::triggers::split_kv_line(rest) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut it = toks.iter();
    let Some(name) = it.next().map(String::as_str) else {
        return "usage: /triggers add <name> network=… channel=… event=TEXT action=slopdrop".into();
    };
    let mut kv: std::collections::HashMap<String, String> = Default::default();
    for tok in it {
        match tok.split_once('=') {
            // An unknown KEY is not a harmless extra: `netwrok=synirc`
            // used to be swallowed here, leaving `network: None` — a
            // row scoped to EVERY network rather than the one typed.
            // `edit` already refused these; `add` was the outlier.
            Some((k, v)) => {
                if let Err(e) = veles_core::triggers::validate_field(k) {
                    return e;
                }
                kv.insert(k.to_string(), v.to_string())
            }
            None => return format!("not a k=v pair: {tok}"),
        };
    }
    let get = |k: &str| kv.get(k).cloned();
    let ev = match veles_core::triggers::validate_event(
        &get("event").unwrap_or_else(|| "TEXT".into()),
    ) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let action = match veles_core::triggers::validate_action(
        &get("action").unwrap_or_else(|| "slopdrop".into()),
    ) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let match_kind = match veles_core::triggers::validate_match(
        &get("match").unwrap_or_else(|| "always".into()),
    ) {
        Ok(m) => m,
        Err(e) => return e,
    };
    // The one nobody checked, and the one whose typo fails OPEN:
    // `evaluate` reads an unparseable floor as Guest.
    let min_role = match get("min-role") {
        Some(r) => match veles_core::triggers::validate_min_role(&r) {
            Ok(r) => Some(r),
            Err(e) => return e,
        },
        None => None,
    };
    let row = veles_core::store::TriggerRow {
        id: 0,
        name: name.to_string(),
        enabled: true,
        platform: get("platform").or(Some(veles_core::triggers::default_platform(&ev).into())),
        network: get("network"),
        channel: get("channel"),
        event: ev,
        match_kind,
        pattern: get("pattern"),
        action,
        arg: get("arg"),
        cooldown_secs: get("cooldown")
            .and_then(|v| v.trim_end_matches('s').parse().ok())
            .unwrap_or(0),
        per_hour_cap: get("cap").and_then(|v| v.parse().ok()).unwrap_or(0),
        min_role,
        fire_count: 0,
        last_fired_at: None,
        last_status: None,
    };
    match store.lock_ok().trigger_upsert(&row) {
        Ok(id) => format!("{name} added (id {id}) — the mask may need /triggers refresh"),
        Err(e) => format!("error: {e}"),
    }
}

/// `/triggers edit <name> k=v…` — partial, None keeps the old value.
fn triggers_edit(
    store: &std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
    name: &str,
    rest: String,
) -> String {
    if name.is_empty() || rest.is_empty() {
        return "usage: /triggers edit <name> k=v…".into();
    }
    let mut network = None;
    let mut channel = None;
    let event = None;
    let match_kind = None;
    // Canonical spellings, owned here so the `&str` borrows below
    // outlive the loop that produced them.
    let mut canon_event: Option<String> = None;
    let mut canon_match: Option<String> = None;
    let mut canon_action: Option<String> = None;
    let mut canon_min_role: Option<String> = None;
    let mut pattern: Option<Option<&str>> = None;
    let action = None;
    let mut arg: Option<Option<&str>> = None;
    let mut cooldown = None;
    let mut cap = None;
    let min_role: Option<Option<&str>> = None;
    let toks = match veles_core::triggers::split_kv_line(&rest) {
        Ok(t) => t,
        Err(e) => return e,
    };
    for tok in &toks {
        let Some((k, v)) = tok.split_once('=') else {
            return format!("not a k=v pair: {tok}");
        };
        // Values, not just keys. This arm rejected an unknown FIELD and
        // then wrote whatever the value was — so `match=contians`
        // landed, and `into_trigger` read it back as `Always`: an
        // image-URL row widened to every line in the channel. Same
        // validators the create surfaces use, so the two cannot drift.
        match k {
            "network" => network = Some(v),
            "channel" => channel = Some(v),
            "event" => match veles_core::triggers::validate_event(v) {
                Ok(e) => canon_event = Some(e),
                Err(e) => return e,
            },
            "match" => match veles_core::triggers::validate_match(v) {
                Ok(m) => canon_match = Some(m),
                Err(e) => return e,
            },
            "pattern" => pattern = Some(Some(v)),
            "action" => match veles_core::triggers::validate_action(v) {
                Ok(a) => canon_action = Some(a),
                Err(e) => return e,
            },
            "arg" => arg = Some(Some(v)),
            "cooldown" => match v.trim_end_matches('s').parse() {
                Ok(n) => cooldown = Some(n),
                Err(_) => return format!("cooldown not a number: {v}"),
            },
            "cap" => match v.parse() {
                Ok(n) => cap = Some(n),
                Err(_) => return format!("cap not a number: {v}"),
            },
            "min-role" => match veles_core::triggers::validate_min_role(v) {
                Ok(r) => canon_min_role = Some(r),
                Err(e) => return e,
            },
            other => {
                return format!(
                    "unknown field '{other}' ({})",
                    veles_core::triggers::FIELDS.join(" ")
                )
            }
        }
    }
    // The canonicalised spellings win over the raw ones: a row written
    // as `Match=ImageURL` must read back the way every other row does.
    let event = canon_event.as_deref().or(event);
    let match_kind = canon_match.as_deref().or(match_kind);
    let action = canon_action.as_deref().or(action);
    let min_role = canon_min_role.as_deref().map(Some).or(min_role);
    match store.lock_ok().trigger_update(
        name, network, channel, event, match_kind, pattern, action, arg, cooldown, cap, min_role,
    ) {
        Ok(Some(t)) => format!(
            "{name} updated ({}/{}/{} {} {} {})",
            t.network.as_deref().unwrap_or("*"),
            t.channel.as_deref().unwrap_or("*"),
            t.event,
            t.match_kind,
            t.action,
            t.cooldown_secs
        ),
        Ok(None) => format!("no trigger named '{name}'"),
        Err(e) => format!("error: {e}"),
    }
}

/// The async half: `refresh` re-derives every channel's forward mask
/// from slopdrop's own live bindings; `mask` sets or clears an
/// override. Everything else delegates to the sync body.
pub async fn triggers_command_async(
    ctx: &AclCtx<'_>,
    cfg: &veles_core::config::VelesConfig,
    rest: &str,
) -> String {
    let mut parts = rest.split_whitespace();
    match parts.next().unwrap_or_default() {
        "refresh" => {
            if !veles_core::triggers::forwarder_configured() {
                return "no trigger forwarder is loaded on this node (the slopdrop module, \
                        with [slopdrop] enabled and a url)"
                    .into();
            }
            // The store is handed as the Arc, not a guard:
            // `force_refresh_masks` locks internally and never holds a
            // guard across its HTTP await (which would make the
            // gateway's spawned future non-Send).
            //
            // `force_`, not the bare `refresh_masks`: it INSTALLS the
            // derived masks into the shared cache the hot path reads.
            // Deriving into a local and reporting it — what this did
            // before — printed the right masks while the IRC path kept
            // using an empty cache and refused every forward. A refresh
            // command that does not refresh is worse than no command.
            match veles_core::triggers::force_refresh_masks(cfg, ctx.store).await {
                Ok(masks) => {
                    let mut out = format!(
                        "masks refreshed from slopdrop ({} channel(s)):\n",
                        masks.len()
                    );
                    let mut keys: Vec<_> = masks.keys().cloned().collect();
                    keys.sort();
                    for k in keys {
                        let m = &masks[&k];
                        let (net, chan) = k.split_once('\u{1}').unwrap_or((k.as_str(), ""));
                        out.push_str(&format!(
                            "  {net} {chan}: {}{}\n",
                            if m.events.is_empty() {
                                "— (nothing forwards)".into()
                            } else {
                                m.events.join(",")
                            },
                            if m.is_derived() {
                                String::new()
                            } else {
                                format!(
                                    " [override: {}]",
                                    m.override_events.as_ref().unwrap().join(",")
                                )
                            },
                        ));
                    }
                    out.trim_end().to_string()
                }
                Err(e) => veles_core::triggers::refresh_failure_report(
                    &e,
                    veles_core::triggers::masks_age_secs(),
                ),
            }
        }
        "mask" => {
            let net = parts.next().unwrap_or_default().to_string();
            let chan = parts.next().unwrap_or_default().to_string();
            let what = parts.next().unwrap_or_default().to_string();
            if net.is_empty() || chan.is_empty() || what.is_empty() {
                return "usage: /triggers mask <network> <channel> <EVENTS|auto>".into();
            }
            let store = ctx.store.lock_ok();
            if what.eq_ignore_ascii_case("auto") {
                match store.trigger_set_mask_override(&net, &chan, None) {
                    // The store write is not enough: the hot path reads
                    // the core mask CACHE, and without this the pin
                    // (or the revert) would not take effect until the
                    // next TTL refresh — up to ten minutes of the
                    // operator watching the command they just ran do
                    // nothing. Overrides are locally determined, so
                    // installing is immediate and needs no slopdrop.
                    Ok(()) => {
                        veles_core::triggers::set_override_in_cache(&net, &chan, None);
                        format!("{net} {chan}: back to the derived mask")
                    }
                    Err(e) => format!("error: {e}"),
                }
            } else {
                let events = what.to_ascii_uppercase();
                for e in events.split(',') {
                    if !matches!(e, "TEXT" | "JOIN" | "PART" | "QUIT" | "KICK" | "NICK") {
                        return format!("unknown event '{e}' in {events}");
                    }
                }
                match store.trigger_set_mask_override(&net, &chan, Some(&events)) {
                    Ok(()) => {
                        veles_core::triggers::set_override_in_cache(
                            &net,
                            &chan,
                            Some(events.split(',').map(|s| s.trim().to_string()).collect()),
                        );
                        format!("{net} {chan}: mask pinned to {events}")
                    }
                    Err(e) => format!("error: {e}"),
                }
            }
        }
        _ => triggers_command(ctx, rest),
    }
}

/// Moved verbatim from the TUI, which was the only surface that had it.
fn acl_listing(ctx: &AclCtx) -> String {
    let grants = veles_core::pairing::list_grants(ctx.store);
    let cfg = acl_cfg(ctx);
    let mut out = String::from("acl: owner surface (full caps)\n");
    out.push_str(&format!("configured: {}\n", cfg.is_configured()));
    if !cfg.owner.is_empty() {
        out.push_str(&format!("owners: {}\n", cfg.owner.join(" ")));
    }
    if let Some(d) = &cfg.default_role {
        out.push_str(&format!("default role: {d}\n"));
    }
    for (id, role) in &cfg.roles {
        out.push_str(&format!("pin: {id} → {role}\n"));
    }
    for e in &cfg.entries {
        out.push_str(&format!("entry: `{}` → {}\n", e.identity, e.role));
    }
    for u in &cfg.users {
        out.push_str(&format!(
            "user: {} → {} [`{}`]\n",
            u.name,
            u.role,
            u.identities.join("` `")
        ));
    }
    // One ACL, two places it can be written: this file and the runtime
    // store. Resolution merges them, so the listing merges them too — but
    // merging is not enough: the operator has to be told which of the two
    // DECIDES, because precedence compares scope before role and role
    // before source, and a file line that merely mentions an identity can
    // still lose to the store's copy of it.
    let now = unix_now();
    let mut store_decides = Vec::new();
    let mut file_decides = Vec::new();
    for g in grants {
        match grant_standing(&g, &cfg, now) {
            Standing::Decides(_) => store_decides.push(g),
            Standing::Inert(_) => file_decides.push(g),
        }
    }
    if store_decides.is_empty() && file_decides.is_empty() {
        out.push_str("runtime grants: none\n");
    }
    if !store_decides.is_empty() {
        out.push_str(
            "runtime grants the STORE decides (lost if the store is reset — \
             `/acl promote` writes them into the file):\n",
        );
        for g in &store_decides {
            out.push_str(&grant_line(g, &cfg, now));
        }
    }
    if !file_decides.is_empty() {
        out.push_str(
            "runtime grants the FILE already decides (dead weight — \
             `/acl promote --prune` drops the copies):\n",
        );
        for g in &file_decides {
            out.push_str(&grant_line(g, &cfg, now));
        }
    }
    // Runtime users are the same story with one more way to be wrong: the
    // file may already carry the NAME, in which case the store copy either
    // repeats it or contradicts it, and only one of those is dead weight.
    let users = veles_core::pairing::list_users(ctx.store);
    let mut u_decides = Vec::new();
    let mut u_inert = Vec::new();
    let mut u_contra = Vec::new();
    for u in &users {
        match user_standing(u, &cfg) {
            UserStanding::Decides(_) => u_decides.push(u),
            UserStanding::Inert(_) => u_inert.push(u),
            UserStanding::Contradicts(_) => u_contra.push(u),
        }
    }
    if users.is_empty() {
        out.push_str("runtime users: none\n");
    }
    if !u_decides.is_empty() {
        out.push_str(
            "runtime users the STORE decides (lost if the store is reset — \
             `/acl promote --users` writes them into the file):\n",
        );
        for u in &u_decides {
            out.push_str(&user_line(u, &cfg));
        }
    }
    if !u_contra.is_empty() {
        out.push_str(
            "runtime users the file CONTRADICTS (it already has the name and says something \
             else — one of the two is a mistake, and promote will not pick):\n",
        );
        for u in &u_contra {
            out.push_str(&user_line(u, &cfg));
        }
    }
    if !u_inert.is_empty() {
        out.push_str(
            "runtime users the FILE already decides (dead weight — \
             `/acl promote --users --prune` drops the copies):\n",
        );
        for u in &u_inert {
            out.push_str(&user_line(u, &cfg));
        }
    }
    out.push_str(
        "subcommands: /acl explain <id|mask> [net] [chan] · list · grant · revoke · \
         promote · user · limits · export",
    );
    out.trim_end().to_string()
}

/// One runtime user, and why it stands the way it does. The reason is the
/// part an operator can act on: "dead weight" without it is a verdict, and
/// a verdict is what the listing used to print for rules that were live.
fn user_line(u: &veles_core::acl::AclUser, cfg: &veles_core::acl::AclConfig) -> String {
    format!(
        "  user `{}` = {} [`{}`]\n    {}\n",
        u.name,
        u.role,
        u.identities.join("` `"),
        user_standing(u, cfg).reason()
    )
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One runtime grant, with the scope it was written with, its expiry when
/// it has one, and WHY it stands the way it does: a grant narrowed to a
/// channel is not the rule an unscoped line declares, and a listing that
/// folds either fact into an identity comparison is one an operator
/// cannot check.
fn grant_line(
    g: &veles_core::pairing::Grant,
    cfg: &veles_core::acl::AclConfig,
    now: u64,
) -> String {
    let scope = scope_phrase(g.network.as_deref(), g.channel.as_deref(), None);
    let scope = if scope == "anywhere" {
        String::new()
    } else {
        format!(" {scope}")
    };
    let expiry = match g.expires_at {
        0 => String::new(),
        t if t > now => format!(", expires {t}"),
        t => format!(", EXPIRED {t}"),
    };
    format!(
        "  `{}`{} = {} (granted {}{expiry})\n    {}\n",
        g.identity,
        scope,
        g.role,
        g.granted_at,
        grant_standing(g, cfg, now).reason()
    )
}

/// "on libera in #veles" — the scope half of a rule, spelled the way
/// `/acl explain` and the listing both spell it.
fn scope_phrase(network: Option<&str>, channel: Option<&str>, platform: Option<&str>) -> String {
    let mut parts = Vec::new();
    if let Some(p) = platform {
        parts.push(format!("on platform {p}"));
    }
    if let Some(n) = network {
        parts.push(format!("on {n}"));
    }
    if let Some(c) = channel {
        parts.push(format!("in {c}"));
    }
    if parts.is_empty() {
        "anywhere".to_string()
    } else {
        parts.join(" ")
    }
}

/// Where a runtime grant stands against the file.
///
/// NOT "does the file mention this identity". `precedence_key` compares
/// exact-identity, then mask specificity, then SCOPE, then the role
/// (lower wins — the fail-closed brake), and only then the source rank
/// that puts config above the store. So an identity the file declares can
/// still be decided by the store: a grant written with a scope beats the
/// file's unscoped line inside that scope, and a grant naming a lower role
/// beats the file's higher one anywhere. Both are real, both are easy to
/// misread as a duplicate, and an operator who deletes the "redundant"
/// store copy on the strength of a listing loses a downgrade they meant or
/// a scope they narrowed.
enum Standing {
    /// A file rule wins every comparison this grant could enter, so the
    /// store copy decides nothing and dropping it changes nothing.
    Inert(String),
    /// The store copy can win somewhere. The reason says where and why.
    Decides(String),
}

impl Standing {
    fn reason(&self) -> &str {
        match self {
            Standing::Inert(r) | Standing::Decides(r) => r,
        }
    }
}

fn grant_standing(
    g: &veles_core::pairing::Grant,
    cfg: &veles_core::acl::AclConfig,
    now: u64,
) -> Standing {
    use veles_core::acl::{Role, RuleSource};
    if g.expires_at != 0 && now > g.expires_at {
        return Standing::Inert(format!(
            "expired at {} — resolution skips it, so nothing depends on this copy",
            g.expires_at
        ));
    }
    let Some(role) = Role::parse(&g.role) else {
        return Standing::Inert(format!(
            "role {:?} does not parse — resolution skips this grant",
            g.role
        ));
    };
    let me = veles_core::acl::RuleSpec {
        identity: g.identity.clone(),
        role,
        network: g.network.clone(),
        channel: g.channel.clone(),
        platform: None,
        source: RuleSource::Grant,
    };
    // The scope is part of the subject: "this grant" is two different
    // rules depending on where it was written, and the sentence that
    // follows has to be checkable against the line above it.
    let subject = format!("this grant{}", scope_suffix(g));
    standing_of(&me, cfg, &subject)
}

/// Where one STORE rule stands against everything the file declares.
///
/// Inert only when every file rule that can meet it reaches all of its
/// callers AND wins the comparison. Both halves matter: a narrower file
/// rule that beats this one inside its own scope leaves it deciding
/// everywhere outside, and a listing that calls that a duplicate is how
/// an operator "cleans up" a rule that was doing work. The comparison is
/// `acl::compare_specs` — the one resolution runs — rather than a
/// retelling of the precedence rules, which is what this function used to
/// be: it compared identity STRINGS, so `octo!*@*` in the store and
/// `octo!*@gateway/veles` in the file were reported as unrelated, and the
/// operator was told the store was the only source for a nick the file
/// bans on one host.
fn standing_of(
    me: &veles_core::acl::RuleSpec,
    cfg: &veles_core::acl::AclConfig,
    subject: &str,
) -> Standing {
    use std::cmp::Ordering;
    use veles_core::acl::{CallerSet, RuleRelation};
    let mut notes: Vec<String> = Vec::new();
    for r in file_rules(cfg) {
        let RuleRelation::Meets {
            callers,
            order,
            edge,
        } = veles_core::acl::compare_specs(me, &r.spec())
        else {
            // No caller can match both, so this file rule says nothing
            // about whether the store copy is needed.
            continue;
        };
        let covered = matches!(callers, CallerSet::Equal | CallerSet::AWithinB);
        if covered && order != Ordering::Greater {
            return Standing::Inert(format!(
                "the file's {} covers every caller {} can reach and wins on {} — dropping the \
                 store copy changes nothing",
                r.describe(),
                subject,
                edge.as_str()
            ));
        }
        notes.push(relation_sentence(&r, callers, order, edge, subject));
    }
    Standing::Decides(if notes.is_empty() {
        format!("no rule in the file can meet {subject} — the store is the only source")
    } else {
        notes.join("\n      ")
    })
}

/// The sentence for a file rule that does NOT make the store copy inert —
/// which is the interesting case, because it has to say what the store
/// copy still decides and where the file takes over from it.
fn relation_sentence(
    r: &FileRule,
    callers: veles_core::acl::CallerSet,
    order: std::cmp::Ordering,
    edge: veles_core::acl::Edge,
    subject: &str,
) -> String {
    use std::cmp::Ordering;
    use veles_core::acl::CallerSet;
    let winner = if order == Ordering::Greater {
        "the store copy"
    } else {
        "the file's rule"
    };
    match callers {
        // The file's rule is the narrower one: everything it reaches, the
        // store rule reaches too, and the store rule reaches more.
        CallerSet::BWithinA => format!(
            "the file's {} is narrower, so {} decides every caller it does not match{}",
            r.describe(),
            subject,
            if order == Ordering::Less {
                format!(" — and loses to it inside, on {}", edge.as_str())
            } else {
                String::new()
            }
        ),
        CallerSet::Overlap => format!(
            "{} overlaps the file's {}: each reaches callers the other does not, and where both \
             match {} wins on {}",
            subject,
            r.describe(),
            winner,
            edge.as_str()
        ),
        // Covered and still standing means the store copy OUTRANKS the
        // file rule that covers it — the downgrade brake, or a scope the
        // file does not pin.
        CallerSet::Equal | CallerSet::AWithinB => format!(
            "{} outranks the file's {} on {} — the file's line decides nothing where the store \
             copy is live",
            subject,
            r.describe(),
            edge.as_str()
        ),
        CallerSet::Unknown => format!(
            "{} and the file's {} cannot be compared from the two specs alone — an exact identity \
             and a mask meet only on a caller carrying both; where both match, {} wins on {}",
            subject,
            r.describe(),
            winner,
            edge.as_str()
        ),
        CallerSet::Disjoint => {
            // `compare_specs` answers Disjoint for these, so this arm is
            // unreachable; said rather than silently mis-described.
            format!("{} cannot meet the file's {}", subject, r.describe())
        }
    }
}

/// Where a runtime USER stands: the same question as a grant, plus one a
/// grant cannot have. Two `[[acl.users]]` tables with one name would leave
/// resolution merging two roles for one subject, so a name the file already
/// uses and says something different about is reported rather than written.
enum UserStanding {
    Inert(String),
    Contradicts(String),
    Decides(String),
}

impl UserStanding {
    fn reason(&self) -> &str {
        match self {
            UserStanding::Inert(r) | UserStanding::Contradicts(r) | UserStanding::Decides(r) => r,
        }
    }
}

fn user_standing(u: &veles_core::acl::AclUser, cfg: &veles_core::acl::AclConfig) -> UserStanding {
    use veles_core::acl::{CallerSet, Role, RuleRelation, RuleSource, RuleSpec};
    let Some(role) = Role::parse(&u.role) else {
        return UserStanding::Inert(format!(
            "role {:?} does not parse — resolution skips this user",
            u.role
        ));
    };
    let extras =
        !u.extra_caps.is_empty() || !u.deny_caps.is_empty() || u.max_context_tokens.is_some();

    if let Some(same) = cfg
        .users
        .iter()
        .find(|f| f.name.trim().eq_ignore_ascii_case(u.name.trim()))
    {
        let file_role = Role::parse(&same.role);
        let covered = |id: &str| {
            file_role.is_some_and(|fr| fr == role)
                && same.identities.iter().any(|f| {
                    matches!(
                        veles_core::acl::compare_specs(
                            &RuleSpec::new(id, role, RuleSource::RuntimeUser),
                            &RuleSpec::new(f, role, RuleSource::User)
                        ),
                        RuleRelation::Meets {
                            callers: CallerSet::Equal | CallerSet::AWithinB,
                            ..
                        }
                    )
                })
        };
        let missing: Vec<&String> = u.identities.iter().filter(|id| !covered(id)).collect();
        if missing.is_empty() && !extras {
            return UserStanding::Inert(format!(
                "the file's user `{}` already carries every one of these identities at {}",
                same.name,
                role.as_str()
            ));
        }
        let mut why = Vec::new();
        if file_role != Some(role) {
            why.push(format!(
                "the file's user `{}` is {} and the store copy is {}",
                same.name,
                file_role
                    .map(|r| r.as_str())
                    .unwrap_or("an unparseable role"),
                role.as_str()
            ));
        }
        if !missing.is_empty() {
            why.push(format!(
                "the store copy adds identities the file's user does not carry: {}",
                missing
                    .iter()
                    .map(|m| format!("`{m}`"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
        if extras {
            why.push(
                "the store copy carries caps or a context ceiling the file's entry does not"
                    .to_string(),
            );
        }
        return UserStanding::Contradicts(why.join("; "));
    }

    // No name in the file, so the user's identities are rules like any
    // other: inert only when every one of them is.
    let mut notes = Vec::new();
    let mut decides = false;
    for id in &u.identities {
        let spec = RuleSpec::new(id, role, RuleSource::RuntimeUser);
        match standing_of(&spec, cfg, &format!("user `{}`'s `{id}`", u.name)) {
            Standing::Inert(r) => notes.push(format!("`{id}`: {r}")),
            Standing::Decides(r) => {
                decides = true;
                notes.push(format!("`{id}`: {r}"));
            }
        }
    }
    if u.identities.is_empty() {
        return UserStanding::Inert(
            "the store copy carries no identities — nothing to write".into(),
        );
    }
    if decides {
        UserStanding::Decides(notes.join("\n      "))
    } else {
        UserStanding::Inert(notes.join("\n      "))
    }
}

/// Every rule the FILE declares, in the shape a comparison needs.
///
/// All four shapes become rules resolution compares — the owner list, the
/// role pins, the scoped entries and the identities a named user collects —
/// so all four are candidates here. Missing one would report a duplicate
/// as store-only, and the operator would "fix" it by writing it twice.
fn file_rules(cfg: &veles_core::acl::AclConfig) -> Vec<FileRule<'_>> {
    use veles_core::acl::{Role, RuleSource};
    let mut out = Vec::new();
    for o in &cfg.owner {
        out.push(FileRule {
            identity: o,
            role: Role::Owner,
            network: None,
            channel: None,
            platform: None,
            source: "owner list".to_string(),
            kind: RuleSource::OwnerList,
        });
    }
    for (spec, role) in &cfg.roles {
        if let Some(role) = Role::parse(role) {
            out.push(FileRule {
                identity: spec,
                role,
                network: None,
                channel: None,
                platform: None,
                source: "role pin".to_string(),
                kind: RuleSource::RolePin,
            });
        }
    }
    for e in &cfg.entries {
        if let Some(role) = Role::parse(&e.role) {
            out.push(FileRule {
                identity: &e.identity,
                role,
                network: e.network.as_deref(),
                channel: e.channel.as_deref(),
                platform: e.platform.as_deref(),
                source: "entry".to_string(),
                kind: RuleSource::Entry,
            });
        }
    }
    for u in &cfg.users {
        if let Some(role) = Role::parse(&u.role) {
            for spec in &u.identities {
                out.push(FileRule {
                    identity: spec,
                    role,
                    network: None,
                    channel: None,
                    platform: None,
                    source: format!("user `{}`", u.name),
                    kind: RuleSource::User,
                });
            }
        }
    }
    out
}

struct FileRule<'a> {
    identity: &'a str,
    role: veles_core::acl::Role,
    network: Option<&'a str>,
    channel: Option<&'a str>,
    platform: Option<&'a str>,
    /// "owner list" · "role pin" · "entry" · "user `ops`" — the shape it
    /// came from, for the sentence that has to explain the verdict.
    source: String,
    /// The same fact as the comparison sees it: where a rule was declared
    /// is the LAST resort in precedence, so a sentence about which rule
    /// wins needs the rank and not just the label.
    kind: veles_core::acl::RuleSource,
}

impl FileRule<'_> {
    /// This rule as the comparison takes it.
    fn spec(&self) -> veles_core::acl::RuleSpec {
        veles_core::acl::RuleSpec {
            identity: self.identity.to_string(),
            role: self.role,
            network: self.network.map(str::to_string),
            channel: self.channel.map(str::to_string),
            platform: self.platform.map(str::to_string),
            source: self.kind,
        }
    }

    /// One phrase naming the rule: what declared it, which identity, what
    /// role, and how far it reaches.
    fn describe(&self) -> String {
        let scope = scope_phrase(self.network, self.channel, self.platform);
        format!(
            "{} `{}` = {}{}",
            self.source,
            self.identity,
            self.role.as_str(),
            if scope == "anywhere" {
                String::new()
            } else {
                format!(" {scope}")
            }
        )
    }
}

/// The ACL the answer should be about.
///
/// The LIVE one when `[acl]` is a file this box actually has, because
/// `configedit` promises `[acl]` edits are in effect now and a report
/// built from the boot snapshot makes that a lie. Falls back to the
/// agent's own config, which is what the tests and a config-less box
/// have.
fn acl_cfg(ctx: &AclCtx) -> veles_core::acl::AclConfig {
    let base = if veles_core::acl::config_file_present() {
        veles_core::acl::live_config()
    } else {
        ctx.fallback.clone()
    };
    veles_core::acl::with_runtime_limits(base, ctx.store)
}

/// What `/acl`, `/sandbox` and `/limits` actually need.
///
/// NOT `&Agent`: the CLI has no agent and building one to answer
/// `veles acl list` would load providers, plugins and a whole session
/// for a question about a rule table. The three commands read a store,
/// a fallback `[acl]` (for a box with no config file), a session id and
/// the caller — and nothing else, which is why this is the seam that
/// lets every surface share one implementation.
pub struct AclCtx<'a> {
    pub store: &'a std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
    /// The `[acl]` to use when there is no config file to be live
    /// about.
    pub fallback: &'a veles_core::acl::AclConfig,
    /// The session whose sandbox `/sandbox use` and `/sandbox where`
    /// are about. Empty on a surface with no session (the CLI, and the
    /// IRC `.agent` path, which answers outside any turn).
    pub session_id: &'a str,
    pub principal: Option<&'a veles_core::acl::Principal>,
    /// Where the caller is, when the surface knows. Without a session
    /// this is what `/sandbox where` resolves the answer from —
    /// otherwise a surface with no session id would be told "the host",
    /// which is the one wrong answer that matters.
    pub sender: Option<&'a veles_core::acl::SenderContext>,
}

impl<'a> AclCtx<'a> {
    /// The view of an in-process agent.
    pub fn from_agent(agent: &'a Agent, principal: Option<&'a veles_core::acl::Principal>) -> Self {
        AclCtx {
            store: &agent.store,
            fallback: &agent.cfg.acl,
            session_id: &agent.session_id,
            principal,
            sender: None,
        }
    }

    /// Same, plus where the caller is.
    pub fn with_sender(mut self, sender: &'a veles_core::acl::SenderContext) -> Self {
        self.sender = Some(sender);
        self
    }
}

/// Who the caller is, for the tier checks inside a subcommand. An
/// owner surface passes no principal and gets the owner's answer.
///
/// This is the field `CommandCtx.principal` was added for and nothing
/// read: shared-command tiering rested entirely on `builtin_cap`, so
/// one command with a read subcommand and a write subcommand had to
/// pick a single tier for both — either the read was locked away from
/// the person it is for, or the write was open to them.
/// `/plugins reload|stop|start|unload <name[:instance]>` — the plugin
/// verbs, on whichever tier hosts the target: a module first, then a
/// process plugin, then a Tcl plugin by name (R7). `None` for the bare
/// listing (and `list`), which the caller renders. `/plugins services`
/// is the service registry's dependency view (`veles_core::services`).
///
/// The listing is `cmd-status` like the rest of `/plugins`; the verbs
/// are `cmd-admin`, because restarting a platform drops every
/// conversation on it. They are filed with the host and answered at
/// once (`request`): the gateway answers chat commands from this sync
/// dispatcher, and a reload can take the plugin's whole shutdown grace.
pub(crate) fn plugins_command(
    principal: Option<&veles_core::acl::Principal>,
    rest: &str,
) -> Option<String> {
    use veles_core::acl::Cap;
    let mut words = rest.split_whitespace();
    let verb = match words.next() {
        None | Some("list") => return None,
        // The service registry's dependency view: who provides each
        // service here, who declares and uses it, what is missing.
        Some("services") => {
            if !caller_is(principal, Cap::CmdStatus) {
                return Some(denied(principal, "`/plugins services`", Cap::CmdStatus));
            }
            return Some(veles_core::services::listing());
        }
        // The override points (PLAN-extension-api.md D2): each decision
        // core asks by name, and who answers it, top first.
        Some("overrides") => {
            if !caller_is(principal, Cap::CmdStatus) {
                return Some(denied(principal, "`/plugins overrides`", Cap::CmdStatus));
            }
            return Some(veles_core::services::overrides_listing());
        }
        Some(v @ ("reload" | "stop" | "start" | "unload")) => v,
        Some(other) => {
            return Some(format!(
                "unknown /plugins verb '{other}' — usage: /plugins [services | overrides | reload|stop|start|unload <name[:instance]>]"
            ))
        }
    };
    if !caller_is(principal, Cap::CmdAdmin) {
        return Some(denied(
            principal,
            &format!("`/plugins {verb}`"),
            Cap::CmdAdmin,
        ));
    }
    let Some(target) = words.next() else {
        return Some(format!("usage: /plugins {verb} <name[:instance]>"));
    };
    Some(plugins_request(target, verb))
}

/// File `verb` for `target` on the tier that hosts it and say where it
/// went. Shared by the chat command and nothing else: the HTTP route
/// waits for the answer instead (`plugins_runtime_control`).
pub(crate) fn plugins_request(target: &str, verb: &str) -> String {
    if let Some(m) = veles_core::modulehost::global() {
        if let Ok(labels) = m.request(target, verb) {
            return format!(
                "{verb} requested for module {} — /plugins shows when it is done",
                labels.join(", ")
            );
        }
    }
    if let Some(h) = veles_core::pluginhost::global() {
        if let Ok(labels) = h.request(target, verb) {
            return format!(
                "{verb} requested for {} — /plugins shows when it is done",
                labels.join(", ")
            );
        }
    }
    if let Some(answer) = script_plugin_request(target, verb) {
        return answer;
    }
    if let Some(answer) = so_plugin_request(target, verb) {
        return answer;
    }
    match tcl_plugin_request(target, verb) {
        Some(answer) => answer,
        None if veles_core::modulehost::global().is_none()
            && veles_core::pluginhost::global().is_none() =>
        {
            format!(
                "plugins run in the gateway, and this process hosts none — \
                 `veles plugins {verb} {target}` asks the running gateway"
            )
        }
        None => format!("no module, process plugin, native or Tcl plugin named `{target}`"),
    }
}

/// The native tier's verbs (R8): `unload`/`stop` tear a `.so` plugin
/// down (every registration purged, its platforms, timers and sockets
/// released, the code unmapped when nothing holds it); `start`/`reload`
/// load it again. Done on a blocking thread (an unload waits, bounded,
/// for a callback to finish); `None` when no enabled native plugin has
/// that name.
fn so_plugin_request(target: &str, verb: &str) -> Option<String> {
    let known = veles_core::rustplugins::so_rows()
        .into_iter()
        .any(|(name, _, _)| name == target);
    if !known {
        return None;
    }
    let name = target.to_string();
    let unload = matches!(verb, "unload" | "stop");
    veles_core::rt::spawn_blocking(move || {
        let r = if unload {
            veles_core::rustplugins::unload_so(&name)
        } else {
            veles_core::rustplugins::start_so(&name)
        };
        match r {
            Ok(msg) => tracing::info!("/plugins: {msg}"),
            Err(e) => tracing::warn!("/plugins {name}: {e:#}"),
        }
    });
    Some(format!(
        "{verb} requested for native plugin {target} — /plugins shows when it is done"
    ))
}

/// The script tier's verbs (R9): a plugin of a script kind (`js`,
/// `racket`, `lisp`) is taken out or loaded by its host through the
/// seam. Filed and answered at once; `None` when no enabled plugin of
/// a script kind has that name.
fn script_plugin_request(target: &str, verb: &str) -> Option<String> {
    let fut = veles_core::scripthost::request(target, verb)?;
    let name = target.to_string();
    let what = verb.to_string();
    veles_core::rt::spawn(async move {
        match fut.await {
            Ok(msg) => tracing::info!("/plugins {what}: {msg}"),
            Err(e) => tracing::warn!("/plugins {what} {name}: {e}"),
        }
    });
    Some(format!(
        "{verb} requested for script plugin {target} — /plugins shows when it is done"
    ))
}

/// The Tcl tier's verbs (R7): `unload`/`stop` drop a plugin's handlers
/// and registrations; `start`/`reload` source it again. Filed on the
/// Tcl thread and answered at once; `None` when no enabled Tcl plugin
/// has that name.
fn tcl_plugin_request(target: &str, verb: &str) -> Option<String> {
    let known = veles_core::tclrt::plugin_rows()
        .into_iter()
        .any(|(name, _)| name == target);
    if !known {
        return None;
    }
    let Some(handle) = veles_core::tclrt::global_handle() else {
        return Some("this process runs no plugins interpreter".into());
    };
    let name = target.to_string();
    let unload = matches!(verb, "unload" | "stop");
    veles_core::rt::spawn(async move {
        let r = if unload {
            handle.unload_plugin(&name).await
        } else {
            handle.load_plugin(&name).await
        };
        match r {
            Ok(msg) => tracing::info!("/plugins: {msg}"),
            Err(e) => tracing::warn!("/plugins {name}: {e}"),
        }
    });
    Some(format!(
        "{verb} requested for Tcl plugin {target} — /plugins shows when it is done"
    ))
}

/// The module instances, appended to the `/plugins` listing the way
/// the process tier's are.
fn modules_listing(host: &veles_core::modulehost::ModuleHost, admin: bool) -> String {
    let all = host.status();
    if all.is_empty() {
        return "\n\nmodules: none".into();
    }
    let mut out = String::from("\n\nmodules:");
    for s in all {
        let label = if s.instance.is_empty() {
            s.plugin.clone()
        } else {
            format!("{}:{}", s.plugin, s.instance)
        };
        out.push_str(&format!(
            "\n{label:<24} {:<8} {}{}",
            s.state,
            if s.version.is_empty() {
                String::new()
            } else {
                format!("v{} ", s.version)
            },
            if s.restarts > 0 {
                format!("({} restarts)", s.restarts)
            } else {
                String::new()
            }
        ));
        if admin {
            if let Some(e) = &s.last_error {
                out.push_str(&format!("\n    last error: {e}"));
            }
        }
    }
    out
}

/// The process-plugin instances, appended to the `/plugins` listing.
/// The last error (paths, a peer's message) is for admins only.
fn process_plugins_listing(host: &veles_core::pluginhost::PluginHost, admin: bool) -> String {
    let all = host.status();
    if all.is_empty() {
        return "\n\nprocess plugins: none running".into();
    }
    let mut out = String::from("\n\nprocess plugins:");
    for s in all {
        let label = if s.instance.is_empty() {
            s.plugin.clone()
        } else {
            format!("{}:{}", s.plugin, s.instance)
        };
        out.push_str(&format!(
            "\n{label:<24} {:<8} {}{}",
            s.state,
            if s.version.is_empty() {
                String::new()
            } else {
                format!("v{} ", s.version)
            },
            if s.restarts > 0 {
                format!("({} restarts)", s.restarts)
            } else {
                String::new()
            }
        ));
        if admin {
            if let Some(e) = &s.last_error {
                out.push_str(&format!("\n    last error: {e}"));
            }
        }
    }
    out
}

fn caller_is(principal: Option<&veles_core::acl::Principal>, cap: veles_core::acl::Cap) -> bool {
    match principal {
        None => true,
        Some(p) => p.has(cap),
    }
}

fn denied(
    principal: Option<&veles_core::acl::Principal>,
    what: &str,
    cap: veles_core::acl::Cap,
) -> String {
    let who = principal
        .map(|p| p.label())
        .unwrap_or_else(|| "this caller".to_string());
    let hint = principal
        .map(|p| veles_core::ticketed::sudo_hint(p, cap))
        .unwrap_or_default();
    format!(
        "[acl] {what} requires the `{}` capability — denied for {who}.{hint}",
        cap.as_str()
    )
}

/// `/acl` and its subcommands — the one implementation every surface
/// reaches.
///
/// The bare form answers "who am I and what may I do", which is open
/// on purpose: knowing your own tier is not a privilege, and the
/// gateway has answered that question since the ACL existed. Reading
/// the RULES is `cmd-admin`; changing them is owner-only, the same
/// floor `/pair`'s management subcommands use, so no surface can
/// become the soft way in.
pub fn acl_command(ctx: &AclCtx, rest: &str) -> String {
    use veles_core::acl::Cap;
    let mut words = rest.split_whitespace();
    let sub = words.next().unwrap_or("");
    let args: Vec<&str> = words.collect();

    match sub {
        "" | "me" | "caps" => match ctx.principal {
            None => acl_listing(ctx),
            Some(p) => {
                let cfg = acl_cfg(ctx);
                let caps = if p.caps.is_empty() {
                    "(none)".to_string()
                } else {
                    p.caps
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                let mut out = format!("you are {} · caps: {caps}", p.label());
                if let Some(v) = &p.via {
                    out.push_str(&format!(" · via {v}"));
                }
                let cap = p.max_context_tokens(&cfg);
                out.push_str(&format!(
                    " · context ceiling {}",
                    if cap == 0 {
                        "unlimited".to_string()
                    } else {
                        format!("{}k tokens", cap / 1000)
                    }
                ));
                out
            }
        },

        "list" | "rules" => {
            if !caller_is(ctx.principal, Cap::CmdAdmin) {
                return denied(ctx.principal, "`/acl list`", Cap::CmdAdmin);
            }
            acl_listing(ctx)
        }

        // WHY this exists at all: precedence is SPECIFICITY now, not
        // declaration order, so "why does jimmy have a dunce cap" is a
        // question about a comparison rather than about a line number.
        // This is the answer, and it names the rules the winner beat.
        "explain" | "why" => {
            if !caller_is(ctx.principal, Cap::CmdAdmin) {
                return denied(ctx.principal, "`/acl explain`", Cap::CmdAdmin);
            }
            let usage = "usage: /acl explain <id|mask> [network] [channel] [platform] \
                         (--network/--channel/--platform also accepted)";
            let Some(who) = args.first() else {
                return usage.into();
            };
            if who.starts_with("--") {
                return format!("`/acl explain`: {who} is a flag, not an identity\n{usage}");
            }
            // WHY flags are accepted here too: `/acl grant` takes
            // `--network n` and this took positionals, so the natural
            // `/acl explain bob --network libera` read `--network` as
            // the NAME of a network and answered "no rule matched" —
            // a confident, plausible, wrong explanation of a question
            // nobody asked. An unknown flag is now refused rather than
            // silently used as a value.
            let mut network = None;
            let mut channel = None;
            let mut platform = None;
            let mut positional: Vec<&str> = Vec::new();
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                let slot: &mut Option<&str> = match *a {
                    "--network" | "--net" => &mut network,
                    "--channel" | "--chan" => &mut channel,
                    "--platform" => &mut platform,
                    other if other.starts_with("--") => {
                        return format!("`/acl explain`: unknown flag {other}\n{usage}");
                    }
                    other => {
                        positional.push(other);
                        continue;
                    }
                };
                let Some(v) = it.next() else {
                    return format!("`/acl explain`: {a} needs a value\n{usage}");
                };
                *slot = Some(v);
            }
            let mut rest = positional.into_iter();
            if network.is_none() {
                network = rest.next();
            }
            if channel.is_none() {
                channel = rest.next();
            }
            if platform.is_none() {
                platform = rest.next();
            }
            if let Some(extra) = rest.next() {
                return format!("`/acl explain`: nothing to do with {extra}\n{usage}");
            }
            acl_explain(ctx, who, network, channel, platform)
        }

        "grant" => {
            if !caller_is(ctx.principal, Cap::All) {
                return denied(ctx.principal, "`/acl grant`", Cap::All);
            }
            let (Some(id), Some(role)) = (args.first(), args.get(1)) else {
                return "usage: /acl grant <id|mask> <role> [--network n] [--channel c] [--ttl-hours h]".into();
            };
            let mut network = None;
            let mut channel = None;
            let mut ttl = None;
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--network" | "--net" => network = it.next().map(|s| s.to_string()),
                    "--channel" | "--chan" => channel = it.next().map(|s| s.to_string()),
                    "--ttl-hours" => {
                        ttl = it
                            .next()
                            .and_then(|s| s.parse::<u64>().ok())
                            .map(|h| h * 3600)
                    }
                    _ => {}
                }
            }
            match veles_core::pairing::grant_scoped(ctx.store, id, role, ttl, network, channel) {
                Ok(g) => format!(
                    "granted: {} = {}{}{}",
                    g.identity,
                    g.role,
                    g.network
                        .as_deref()
                        .map(|n| format!(" on {n}"))
                        .unwrap_or_default(),
                    g.channel
                        .as_deref()
                        .map(|c| format!(" in {c}"))
                        .unwrap_or_default()
                ),
                Err(e) => format!("grant failed: {e:#}"),
            }
        }

        "revoke" => {
            if !caller_is(ctx.principal, Cap::All) {
                return denied(ctx.principal, "`/acl revoke`", Cap::All);
            }
            let Some(id) = args.first() else {
                return "usage: /acl revoke <id|mask>".into();
            };
            if veles_core::pairing::revoke(ctx.store, id) {
                format!("revoked: {id}")
            } else {
                format!("no live grant for {id} (config rules are edited in config.toml)")
            }
        }

        // A live decision that turned out to be permanent should not stay
        // in a store a reset takes away. `/acl export` prints the lines for
        // a human to paste; this writes them through the same
        // comment-preserving merge `config fill` uses and then drops the
        // store copies, so the file becomes the one source of truth the
        // listing claims it is.
        "promote" => {
            if !caller_is(ctx.principal, Cap::All) {
                return denied(ctx.principal, "`/acl promote`", Cap::All);
            }
            acl_promote(ctx, &args)
        }

        "user" => {
            let action = args.first().copied().unwrap_or("");
            match action {
                "list" | "" => {
                    if !caller_is(ctx.principal, Cap::CmdAdmin) {
                        return denied(ctx.principal, "`/acl user list`", Cap::CmdAdmin);
                    }
                    let users = veles_core::pairing::list_users(ctx.store);
                    if users.is_empty() {
                        return "no runtime users (config `[[acl.users]]` shows in /acl list)"
                            .into();
                    }
                    let mut out = String::from("runtime users:\n");
                    for u in users {
                        out.push_str(&format!(
                            "  {} = {} [{}]\n",
                            u.name,
                            u.role,
                            u.identities.join(" ")
                        ));
                    }
                    out.trim_end().to_string()
                }
                "save" | "add" => {
                    if !caller_is(ctx.principal, Cap::All) {
                        return denied(ctx.principal, "`/acl user save`", Cap::All);
                    }
                    let (Some(name), Some(role)) = (args.get(1), args.get(2)) else {
                        return "usage: /acl user save <name> <role> <id|mask> [more…]".into();
                    };
                    let ids: Vec<String> = args[3..].iter().map(|s| s.to_string()).collect();
                    if ids.is_empty() {
                        return "a user needs at least one identity or mask".into();
                    }
                    match veles_core::pairing::user_save(ctx.store, name, role, &ids) {
                        Ok(u) => format!(
                            "user {} = {} [{}] — one subject across every network they match",
                            u.name,
                            u.role,
                            u.identities.join(" ")
                        ),
                        Err(e) => format!("user save failed: {e:#}"),
                    }
                }
                "rm" | "delete" => {
                    if !caller_is(ctx.principal, Cap::All) {
                        return denied(ctx.principal, "`/acl user rm`", Cap::All);
                    }
                    let Some(name) = args.get(1) else {
                        return "usage: /acl user rm <name>".into();
                    };
                    if veles_core::pairing::user_delete(ctx.store, name) {
                        format!("removed runtime user {name}")
                    } else {
                        format!("no runtime user named {name}")
                    }
                }
                other => format!("unknown /acl user subcommand {other:?} (list|save|rm)"),
            }
        }

        "limits" => limits_command(ctx, &args.join(" ")),

        // Pasteable TOML for the LIVE rules, so a live grant that
        // turned out to be permanent can become a config line without
        // being retyped from a listing. `--write` does the writing, and
        // it does it by being `/acl promote`: one writer into
        // config.toml, one set of refusals, no second path that has to
        // learn them again.
        "export" => {
            let mut write = false;
            let mut passthrough: Vec<&str> = Vec::new();
            for a in args {
                match a {
                    "--write" => write = true,
                    "--dry-run" | "-n" | "--include-expiring" | "--users" | "--prune" => {
                        passthrough.push(a)
                    }
                    other => {
                        return format!(
                            "unknown flag {other:?} — usage: /acl export [--write] \
                             [--dry-run] [--include-expiring] [--users] [--prune]"
                        )
                    }
                }
            }
            if !write {
                if !passthrough.is_empty() {
                    return format!(
                        "{} only mean something with --write — usage: /acl export [--write] \
                         [--dry-run] [--include-expiring] [--users] [--prune]",
                        passthrough.join(" ")
                    );
                }
                if !caller_is(ctx.principal, Cap::CmdAdmin) {
                    return denied(ctx.principal, "`/acl export`", Cap::CmdAdmin);
                }
                return acl_export(ctx);
            }
            // Writing config.toml is the owner's business even though
            // READING the live rules is an admin's.
            if !caller_is(ctx.principal, Cap::All) {
                return denied(ctx.principal, "`/acl export --write`", Cap::All);
            }
            // Everything the store holds, since "export" promises the
            // whole view: expiring grants included, users included.
            let mut flags = vec!["--include-expiring", "--users"];
            flags.extend(passthrough);
            format!(
                "`/acl export --write` is `/acl promote {}` — one writer, one set of \
                 refusals:\n{}",
                flags.join(" "),
                acl_promote(ctx, &flags)
            )
        }

        other => format!(
            "unknown /acl subcommand {other:?} — me | list | explain | grant | revoke | \
             promote | user | limits | export"
        ),
    }
}

/// `/acl explain` — resolve a hypothetical caller and show the whole
/// comparison, winner first.
fn acl_explain(
    ctx: &AclCtx,
    who: &str,
    network: Option<&str>,
    channel: Option<&str>,
    platform: Option<&str>,
) -> String {
    let cfg = acl_cfg(ctx);
    let is_mask = veles_core::ircmask::looks_like_mask(who);
    let ctx = veles_core::acl::SenderContext {
        mask: is_mask.then(|| who.to_string()),
        network: network.map(str::to_string),
        channel: channel.map(str::to_string),
        platform: platform
            .map(str::to_string)
            .or_else(|| is_mask.then(|| "irc".to_string())),
        chat_id: channel.map(str::to_string),
        direct: channel.is_none(),
        handle: None,
        post: None,
    };
    let id = (!is_mask).then_some(who);
    let rules = veles_core::acl::Principal::explain(id, &ctx, &cfg);
    let p = veles_core::acl::Principal::resolve_ctx(id, &ctx, &cfg);

    let mut out = format!("`{who}` resolves to {}", p.label());
    if let Some(v) = &p.via {
        out.push_str(&format!(" (via {v})"));
    }
    let ceiling = p.max_context_tokens(&cfg);
    out.push_str(&format!(
        "\ncontext ceiling: {}",
        if ceiling == 0 {
            "unlimited".to_string()
        } else {
            format!("{}k tokens", ceiling / 1000)
        }
    ));
    if rules.is_empty() {
        out.push_str(&format!(
            "\nno rule matched — the default role applies ({})",
            cfg.default_role.as_deref().unwrap_or("guest")
        ));
        // …except where the connector does not admit unnamed senders at
        // all, in which case the role above is academic and saying only
        // the role would be a half-truth. USER, 2026-09-16: *"is it
        // still resolving people as guests on telegram?"* — this is the
        // command that should have answered that.
        match ctx.platform.as_deref() {
            Some(plat) => match cfg.strangers_on(Some(plat)) {
                veles_core::acl::StrangerPolicy::Ignore => out.push_str(&format!(
                    "\n…but [acl.strangers] {plat} = \"ignore\": on that connector this \
                     sender never reaches a turn at all. Name them in [[acl.users]] or \
                     [[acl.entries]] to let them in."
                )),
                veles_core::acl::StrangerPolicy::Role(r) => out.push_str(&format!(
                    "\n…except on {plat}, where [acl.strangers] makes an unnamed sender {}",
                    r.as_str()
                )),
                veles_core::acl::StrangerPolicy::DefaultRole => {}
            },
            None if !cfg.strangers.is_empty() => out.push_str(
                "\n(some connectors carry an [acl.strangers] door of their own — pass \
                 --platform to see this sender's answer there)",
            ),
            None => {}
        }
        return out;
    }
    out.push_str("\nrules, most specific first:");
    for (i, r) in rules.iter().enumerate() {
        out.push_str(&format!(
            "\n  {} {}",
            if i == 0 { "→" } else { " " },
            r.describe()
        ));
    }
    if rules.len() > 1 {
        out.push_str(
            "\n(the narrowest match wins regardless of declaration order; a tie at equal \
             specificity goes to the LOWER role)",
        );
    }
    out
}

/// Live rules as TOML.
fn acl_export(ctx: &AclCtx) -> String {
    let mut out = String::from(
        "# live ACL rules, pasteable into config.toml\n\
         # (`/acl export --write` folds the store's rules in for you, through the same\n\
         #  writer `/acl promote` uses — same refusals, same backup)\n",
    );
    let cfg = acl_cfg(ctx);
    out.push_str("[acl]\n");
    if !cfg.owner.is_empty() {
        out.push_str(&format!(
            "owner = [{}]\n",
            cfg.owner
                .iter()
                .map(|o| format!("{o:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(d) = &cfg.default_role {
        out.push_str(&format!("default_role = {d:?}\n"));
    }
    out.push_str(&format!(
        "\n[acl.limits]\ndefault_max_context_tokens = {}\n",
        cfg.limits.default_max_context_tokens
    ));
    if !cfg.limits.by_role.is_empty() {
        out.push_str("\n[acl.limits.by_role]\n");
        let mut rows: Vec<(&String, &u64)> = cfg.limits.by_role.iter().collect();
        rows.sort();
        for (role, n) in rows {
            out.push_str(&format!("{role} = {n}\n"));
        }
    }
    for e in &cfg.entries {
        out.push_str(&format!(
            "\n[[acl.entries]]\nidentity = {:?}\nrole = {:?}\n",
            e.identity, e.role
        ));
        for (k, v) in [
            ("network", &e.network),
            ("channel", &e.channel),
            ("platform", &e.platform),
        ] {
            if let Some(v) = v {
                out.push_str(&format!("{k} = {v:?}\n"));
            }
        }
        if let Some(n) = e.max_context_tokens {
            out.push_str(&format!("max_context_tokens = {n}\n"));
        }
    }
    // Runtime users and grants, written in the shape config declares —
    // which is the whole point of the export: a live decision that
    // turned out to be permanent should not have to be retyped.
    for u in cfg
        .users
        .iter()
        .cloned()
        .chain(veles_core::pairing::list_users(ctx.store))
    {
        out.push_str(&format!(
            "\n[[acl.users]]\nname = {:?}\nrole = {:?}\nidentities = [{}]\n",
            u.name,
            u.role,
            u.identities
                .iter()
                .map(|i| format!("{i:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    for g in veles_core::pairing::list_grants(ctx.store) {
        out.push_str(&format!(
            "\n# was a live grant ({}){}\n[[acl.entries]]\nidentity = {:?}\nrole = {:?}\n",
            g.granted_at,
            if g.expires_at != 0 {
                format!(", expiring {}", g.expires_at)
            } else {
                String::new()
            },
            g.identity,
            g.role
        ));
        if let Some(n) = &g.network {
            out.push_str(&format!("network = {n:?}\n"));
        }
        if let Some(c) = &g.channel {
            out.push_str(&format!("channel = {c:?}\n"));
        }
    }
    out
}

/// `/acl promote [--dry-run] [--include-expiring] [--users] [--prune]` —
/// write the runtime rules the store decides into `config.toml`, then drop
/// the store copies.
///
/// Four refusals are the point of the command rather than edges of it:
///
/// - A rule the FILE already decides is not written. Writing it would add a
///   second rule for one identity and make the file look like it has two
///   opinions; `--prune` drops the dead store copy instead. "Already
///   decides" is `acl::compare_specs` — the comparison resolution itself
///   runs — over the two rules' callers as well as their precedence, so a
///   grant the file's narrower mask only PARTLY covers is written, with the
///   sentence saying which callers each of the two keeps.
/// - A grant that still has an expiry is not written by default, because
///   `[[acl.entries]]` has no expiry: promoting one silently turns "trusted
///   for 24 hours" into "trusted forever". `--include-expiring` does it
///   anyway and the report says the expiry was dropped.
/// - Runtime users are written only with `--users`, and never over a name
///   the file already uses for something else. Two `[[acl.users]]` tables
///   sharing a name would leave resolution merging two roles for one
///   subject — an edit for a human who can read both, not for a
///   maintenance command.
/// - Nothing is written at all when `[acl]` is unconfigured. Today every
///   sender the adapter allows is OWNER there; the first `[[acl.entries]]`
///   or `[[acl.users]]` line makes the ACL curated and every identity the
///   file does not name falls to `guest` — the operator included. A
///   maintenance command has no business being the thing that locks
///   somebody out of their own bot.
fn acl_promote(ctx: &AclCtx, args: &[&str]) -> String {
    let mut dry = false;
    let mut include_expiring = false;
    let mut with_users = false;
    let mut prune = false;
    for a in args {
        match *a {
            "--dry-run" | "-n" => dry = true,
            "--include-expiring" => include_expiring = true,
            "--users" => with_users = true,
            "--prune" => prune = true,
            other => {
                return format!(
                    "unknown flag {other:?} — usage: /acl promote [--dry-run] \
                     [--include-expiring] [--users] [--prune]"
                )
            }
        }
    }

    let cfg = acl_cfg(ctx);
    let now = unix_now();
    let grants = veles_core::pairing::list_grants(ctx.store);
    let mut to_write = Vec::new();
    let mut inert = Vec::new();
    let mut temporary = Vec::new();
    for g in &grants {
        match grant_standing(g, &cfg, now) {
            Standing::Inert(_) => inert.push(g),
            Standing::Decides(_) => {
                if g.expires_at != 0 && !include_expiring {
                    temporary.push(g);
                } else {
                    to_write.push(g);
                }
            }
        }
    }

    let runtime_users = veles_core::pairing::list_users(ctx.store);
    let mut users_write = Vec::new();
    let mut users_inert = Vec::new();
    let mut users_contra = Vec::new();
    if with_users {
        for u in &runtime_users {
            match user_standing(u, &cfg) {
                UserStanding::Inert(_) => users_inert.push(u),
                UserStanding::Contradicts(_) => users_contra.push(u),
                UserStanding::Decides(_) => users_write.push(u),
            }
        }
    }

    let notes: Vec<String> = to_write.iter().map(|g| promote_note(g, now)).collect();
    let block: String = to_write
        .iter()
        .zip(&notes)
        .map(|(g, note)| entry_block(g, note))
        .chain(users_write.iter().map(|u| user_block(u)))
        .collect();

    // The legacy-mode refusal, before anything touches the file.
    if (!to_write.is_empty() || !users_write.is_empty()) && !cfg.is_configured() {
        let why = "refusing to promote: `[acl]` is not configured, so every sender the \
                   adapter allows is OWNER today. The first `[[acl.entries]]` or \
                   `[[acl.users]]` line makes the ACL curated, and every identity the file \
                   does not name — yours included — falls to `guest`. Put an owner list in \
                   the file first (`veles config fill` documents the shape), then promote \
                   again."
            .to_string();
        return if dry {
            format!("dry run: would refuse.\n{why}\n\nthe lines it was asked to write:\n{block}")
        } else {
            why
        };
    }

    let mut out = String::new();
    let mut dropped: Vec<String> = Vec::new();

    if !to_write.is_empty() || !users_write.is_empty() {
        if dry {
            out.push_str(&format!(
                "dry run — nothing written. would append to {}:\n{block}",
                veles_core::configedit::config_path().display()
            ));
        } else {
            let notes = notes.clone();
            let write = to_write.clone();
            let users = users_write.clone();
            let hint = if to_write.is_empty() {
                "acl.users"
            } else {
                "acl.entries"
            };
            match veles_core::configedit::edit(hint, move |doc| {
                for (g, note) in write.iter().zip(&notes) {
                    append_acl_entry(doc, g, note)?;
                }
                for u in &users {
                    append_acl_user(doc, u)?;
                }
                Ok(())
            }) {
                Ok(report) => {
                    // `[acl]` is re-read on every use, but this process may
                    // have cached it a filesystem tick ago.
                    veles_core::acl::invalidate_live_config();
                    for g in &to_write {
                        if veles_core::pairing::revoke(ctx.store, &g.identity) {
                            dropped.push(format!("`{}`", g.identity));
                        }
                    }
                    for u in &users_write {
                        if veles_core::pairing::user_delete(ctx.store, &u.name) {
                            dropped.push(format!("user `{}`", u.name));
                        }
                    }
                    let mut what = Vec::new();
                    if !to_write.is_empty() {
                        what.push(counted(to_write.len(), "runtime grant"));
                    }
                    if !users_write.is_empty() {
                        what.push(counted(users_write.len(), "runtime user"));
                    }
                    out.push_str(&format!(
                        "promoted {} into {} — {}{}\n",
                        what.join(" and "),
                        report.path.display(),
                        veles_core::configedit::effect_for(hint).note(),
                        report
                            .backup
                            .map(|b| format!(" (backup: {})", b.display()))
                            .unwrap_or_default()
                    ));
                    // Every line says why it was worth writing. "It was in
                    // the store" is not a reason, and the operator who
                    // promoted a rule that overlaps one already in the file
                    // needs to be told which callers each of them keeps
                    // while that is still a question anybody can answer.
                    for g in &to_write {
                        out.push_str(&format!(
                            "  + `{}` = {}{}\n",
                            g.identity,
                            g.role,
                            if g.expires_at != 0 {
                                format!(" — expiry {} dropped", g.expires_at)
                            } else {
                                scope_suffix(g)
                            }
                        ));
                        if let Standing::Decides(why) = grant_standing(g, &cfg, now) {
                            out.push_str(&format!("      {why}\n"));
                        }
                    }
                    for u in &users_write {
                        out.push_str(&format!(
                            "  + user `{}` = {} [{}]\n",
                            u.name,
                            u.role,
                            u.identities.join(" ")
                        ));
                        if let UserStanding::Decides(why) = user_standing(u, &cfg) {
                            out.push_str(&format!("      {why}\n"));
                        }
                    }
                    if !dropped.is_empty() {
                        out.push_str(&format!("store copies dropped: {}\n", dropped.join(" ")));
                    }
                }
                Err(e) => return format!("promote failed, config.toml untouched: {e:#}"),
            }
        }
    }

    if !temporary.is_empty() {
        out.push_str(&format!(
            "left in the store ({}): {}\n  a config entry cannot expire, so \
             promoting one would make it permanent — --include-expiring writes it anyway\n",
            counted(temporary.len(), "temporary grant"),
            temporary
                .iter()
                .map(|g| format!("`{}` = {} until {}", g.identity, g.role, g.expires_at))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    // Users nobody asked about still deserve a line: a store the listing
    // calls the source of truth should not be holding rules this command
    // quietly walked past.
    if !with_users && !runtime_users.is_empty() {
        out.push_str(&format!(
            "left in the store ({}): {}\n  promote writes grants; --users folds runtime \
             users into [[acl.users]] too\n",
            counted(runtime_users.len(), "runtime user"),
            runtime_users
                .iter()
                .map(|u| format!("`{}` = {}", u.name, u.role))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }

    if !inert.is_empty() {
        if prune && !dry {
            for g in &inert {
                if veles_core::pairing::revoke(ctx.store, &g.identity) {
                    dropped.push(format!("`{}`", g.identity));
                }
            }
            out.push_str(&format!(
                "pruned {} the file already decides: {}\n",
                counted(inert.len(), "grant"),
                inert
                    .iter()
                    .map(|g| format!("`{}`", g.identity))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        } else {
            out.push_str(&format!(
                "{} the file already decides (nothing to write — --prune drops the store \
                 {}):\n",
                counted(inert.len(), "grant"),
                if inert.len() == 1 { "copy" } else { "copies" }
            ));
            for g in &inert {
                out.push_str(&format!(
                    "  `{}` = {} — {}\n",
                    g.identity,
                    g.role,
                    grant_standing(g, &cfg, now).reason()
                ));
            }
        }
    }

    if !users_inert.is_empty() {
        if prune && !dry {
            for u in &users_inert {
                if veles_core::pairing::user_delete(ctx.store, &u.name) {
                    dropped.push(format!("user `{}`", u.name));
                }
            }
            out.push_str(&format!(
                "pruned {} the file already decides: {}\n",
                counted(users_inert.len(), "runtime user"),
                users_inert
                    .iter()
                    .map(|u| format!("`{}`", u.name))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        } else {
            out.push_str(&format!(
                "{} the file already decides (nothing to write — --prune drops the store \
                 {}):\n",
                counted(users_inert.len(), "runtime user"),
                if users_inert.len() == 1 {
                    "copy"
                } else {
                    "copies"
                }
            ));
            for u in &users_inert {
                out.push_str(&format!(
                    "  user `{}` = {} — {}\n",
                    u.name,
                    u.role,
                    user_standing(u, &cfg).reason()
                ));
            }
        }
    }

    if !users_contra.is_empty() {
        out.push_str(&format!(
            "{} NOT written, because the file already has the name and says something else \
             (edit config.toml, or `/acl user rm` the copy):\n",
            counted(users_contra.len(), "runtime user")
        ));
        for u in &users_contra {
            out.push_str(&format!(
                "  user `{}` = {} — {}\n",
                u.name,
                u.role,
                user_standing(u, &cfg).reason()
            ));
        }
    }

    if prune && !dry && !dropped.is_empty() && to_write.is_empty() && users_write.is_empty() {
        out.push_str(&format!("store copies dropped: {}\n", dropped.join(" ")));
    }

    if out.trim().is_empty() {
        return "nothing to promote: the store holds no runtime grants or users".to_string();
    }
    out.trim_end().to_string()
}

/// "2 runtime grants" — the report reads as a sentence, so the counting
/// is one helper rather than four inline `if n == 1` branches.
fn counted(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn scope_suffix(g: &veles_core::pairing::Grant) -> String {
    let s = scope_phrase(g.network.as_deref(), g.channel.as_deref(), None);
    if s == "anywhere" {
        String::new()
    } else {
        format!(" {s}")
    }
}

/// The comment the promoted line carries, so the file says where the rule
/// came from instead of looking hand-written.
fn promote_note(g: &veles_core::pairing::Grant, now: u64) -> String {
    let expiry = match g.expires_at {
        0 => String::new(),
        t if t > now => format!(", expiry {t} dropped — a config entry cannot expire"),
        t => format!(", was expired {t}"),
    };
    format!("was a runtime grant (granted {}{expiry})", g.granted_at)
}

/// The pasteable shape of one promoted grant — the same text `/acl export`
/// prints, so a dry run shows exactly what a real one writes.
fn entry_block(g: &veles_core::pairing::Grant, note: &str) -> String {
    let mut out = format!(
        "\n# {note}\n[[acl.entries]]\nidentity = {:?}\nrole = {:?}\n",
        g.identity, g.role
    );
    if let Some(n) = &g.network {
        out.push_str(&format!("network = {n:?}\n"));
    }
    if let Some(c) = &g.channel {
        out.push_str(&format!("channel = {c:?}\n"));
    }
    out
}

/// The pasteable shape of one promoted user — the same text `/acl export`
/// prints, so a dry run shows exactly what a real one writes.
fn user_block(u: &veles_core::acl::AclUser) -> String {
    let mut out = format!(
        "\n# was a runtime user\n[[acl.users]]\nname = {:?}\nrole = {:?}\nidentities = [{}]\n",
        u.name,
        u.role,
        u.identities
            .iter()
            .map(|i| format!("{i:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (key, caps) in [("extra_caps", &u.extra_caps), ("deny_caps", &u.deny_caps)] {
        if !caps.is_empty() {
            out.push_str(&format!(
                "{key} = [{}]\n",
                caps.iter()
                    .map(|c| format!("{c:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if let Some(n) = u.max_context_tokens {
        out.push_str(&format!("max_context_tokens = {n}\n"));
    }
    out
}

/// Append one `[[acl.entries]]` table to the document, creating `[acl]` and
/// the array when the file has neither. Everything else in the document —
/// comments, key order, the operator's own formatting — is untouched,
/// because `configedit` never re-serializes.
fn append_acl_entry(
    doc: &mut toml_edit::DocumentMut,
    g: &veles_core::pairing::Grant,
    note: &str,
) -> anyhow::Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table};
    let root = doc.as_table_mut();
    if root.get("acl").map(|i| i.is_none()).unwrap_or(true) {
        root.insert("acl", Item::Table(Table::new()));
    }
    let acl = root
        .get_mut("acl")
        .and_then(|i| i.as_table_like_mut())
        .ok_or_else(|| anyhow::anyhow!("`acl` is not a table"))?;
    if acl.get("entries").map(|i| i.is_none()).unwrap_or(true) {
        acl.insert("entries", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let arr = acl
        .get_mut("entries")
        .and_then(|i| i.as_array_of_tables_mut())
        .ok_or_else(|| anyhow::anyhow!("`acl.entries` is not an array of tables"))?;
    let mut t = Table::new();
    t["identity"] = toml_edit::value(g.identity.clone());
    t["role"] = toml_edit::value(g.role.clone());
    if let Some(n) = &g.network {
        t["network"] = toml_edit::value(n.clone());
    }
    if let Some(c) = &g.channel {
        t["channel"] = toml_edit::value(c.clone());
    }
    t.decor_mut().set_prefix(format!("\n# {note}\n"));
    arr.push(t);
    Ok(())
}

/// Append one `[[acl.users]]` table the same way. The caps and the context
/// ceiling are written only when the runtime user actually carries them, so
/// a promoted user is the rule the store was enforcing and not a broader
/// one — a user who had `deny_caps` in the store must not arrive in the
/// file without them.
fn append_acl_user(
    doc: &mut toml_edit::DocumentMut,
    u: &veles_core::acl::AclUser,
) -> anyhow::Result<()> {
    use toml_edit::{ArrayOfTables, Item, Table};
    let root = doc.as_table_mut();
    if root.get("acl").map(|i| i.is_none()).unwrap_or(true) {
        root.insert("acl", Item::Table(Table::new()));
    }
    let acl = root
        .get_mut("acl")
        .and_then(|i| i.as_table_like_mut())
        .ok_or_else(|| anyhow::anyhow!("`acl` is not a table"))?;
    if acl.get("users").map(|i| i.is_none()).unwrap_or(true) {
        acl.insert("users", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let arr = acl
        .get_mut("users")
        .and_then(|i| i.as_array_of_tables_mut())
        .ok_or_else(|| anyhow::anyhow!("`acl.users` is not an array of tables"))?;
    let mut t = Table::new();
    t["name"] = toml_edit::value(u.name.clone());
    t["role"] = toml_edit::value(u.role.clone());
    t["identities"] = toml_edit::value(str_array(&u.identities));
    if !u.extra_caps.is_empty() {
        t["extra_caps"] = toml_edit::value(str_array(&u.extra_caps));
    }
    if !u.deny_caps.is_empty() {
        t["deny_caps"] = toml_edit::value(str_array(&u.deny_caps));
    }
    if let Some(n) = u.max_context_tokens {
        t["max_context_tokens"] = toml_edit::value(n as i64);
    }
    t.decor_mut().set_prefix("\n# was a runtime user\n");
    arr.push(t);
    Ok(())
}

fn str_array(items: &[String]) -> toml_edit::Array {
    let mut arr = toml_edit::Array::new();
    for i in items {
        arr.push(i.clone());
    }
    arr
}

// ── /gen: media generation as a first-class act ──────────────────────
//
// USER, 2026-09-11: commands "dedicated to image/video/audio generation
// letting the user specify the provider and/or model out of available
// ones, gated behind user+ acls". The model already has the tools; what
// was missing is the user's OWN door — one that costs no turn, cannot
// be talked out of, and says which route it spent the prompt on. The
// result carries a `MEDIA:` path, so each surface's existing delivery
// (and its gate) handles the rest exactly as it would a reply's.

/// What `/gen` was asked to make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenKind {
    Image,
    Video,
    Audio,
}

/// One parsed `/gen` invocation. Pure — the parse is where user input
/// turns into structure, so it is where the tests live.
pub(crate) struct GenRequest {
    pub kind: GenKind,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub voice: Option<String>,
    pub speed: Option<f64>,
    pub list: bool,
    pub prompt: String,
}

/// Parse `/gen <kind> [route] [flags] prompt…`.
///
/// The route shorthand is deliberately positional: `comfyui` alone, or
/// `provider/model` (the model is everything after the FIRST slash —
/// `router/google/image-x` names provider `router`, model
/// `google/image-x`). A first word that is neither a kind, a flag, nor
/// a route is just the prompt's first word; `/gen draw me a cat` is a
/// missing kind and gets the usage, not a guess, because "did you mean
/// image" is a reply away and a wrong generation is a bill.
///
/// NOT the same rule as `/model`'s `provider/model`, and the two must
/// not be "unified". Here the split is safe because it is POSITIONAL:
/// only the first word, only before any prompt word has accumulated, in
/// a grammar whose first word is a kind or a route and nothing else.
/// `/model` takes one bare string with no position to lean on, so it
/// resolves the whole name as a model first and splits only if nothing
/// claimed it (`Agent::set_model_inner`, step 3.5). Flattening either
/// into the other turns `openrouter/free` into a 404.
pub(crate) fn parse_gen(rest: &str) -> Option<GenRequest> {
    let mut kind: Option<GenKind> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut voice: Option<String> = None;
    let mut speed: Option<f64> = None;
    let mut list = false;
    let mut prompt: Vec<&str> = Vec::new();

    let mut words = rest.split_whitespace().peekable();
    // The kind word first, if there is one.
    if let Some(w) = words.peek() {
        let k = match w.to_ascii_lowercase().as_str() {
            "image" | "img" | "i" => Some(GenKind::Image),
            "video" | "vid" | "v" => Some(GenKind::Video),
            "audio" | "tts" | "voice" | "speech" | "say" => Some(GenKind::Audio),
            _ => None,
        };
        if let Some(k) = k {
            kind = Some(k);
            words.next();
        }
    }
    let kind = kind?;

    while let Some(w) = words.next() {
        let w = w.trim();
        if w.is_empty() {
            continue;
        }
        match w {
            "--list" | "-l" => list = true,
            "--provider" | "--route" => provider = words.next().map(String::from),
            "--model" | "-m" => model = words.next().map(String::from),
            "--voice" => voice = words.next().map(String::from),
            "--speed" => speed = words.next().and_then(|s| s.parse().ok()),
            _ if w.starts_with("--provider=") => {
                provider = Some(w["--provider=".len()..].to_string())
            }
            _ if w.starts_with("--route=") => provider = Some(w["--route=".len()..].to_string()),
            _ if w.starts_with("--model=") => model = Some(w["--model=".len()..].to_string()),
            _ if w.starts_with("--voice=") => voice = Some(w["--voice=".len()..].to_string()),
            // The route shorthand only makes sense before the prompt
            // starts; once words are being made, `router/x` is speech.
            _ if prompt.is_empty() => {
                if let Some((p, m)) = w.split_once('/') {
                    if !p.is_empty() && !m.is_empty() && !p.starts_with('-') {
                        provider = Some(p.to_string());
                        model = Some(m.to_string());
                        continue;
                    }
                }
                if w.eq_ignore_ascii_case("comfyui") || w.eq_ignore_ascii_case("edge") {
                    provider = Some(w.to_ascii_lowercase());
                    continue;
                }
                prompt.push(w);
            }
            _ => prompt.push(w),
        }
    }

    Some(GenRequest {
        kind,
        provider,
        model,
        voice,
        speed,
        list,
        prompt: prompt.join(" "),
    })
}

/// The gate on its own, so a test can hold it without an Agent:
/// every kind of generation is `media-gen`, which is a USER cap as of
/// 2026-09-11 — guests are read-only by directive, and a generation
/// spends provider money.
pub(crate) fn gen_denied(principal: Option<&veles_core::acl::Principal>) -> Option<String> {
    if caller_is(principal, veles_core::acl::Cap::MediaGen) {
        None
    } else {
        Some(denied(principal, "`/gen`", veles_core::acl::Cap::MediaGen))
    }
}

/// Claim one unit of a per-hour budget for this caller, or render the
/// refusal.
///
/// The brakes the gateway applies to TURNS (`claim_turn` + the
/// concurrency permit) sit below every command branch on purpose, so
/// `/status` is never billed as a turn. `/gen` and `/tcl` are answered in
/// that same region and inherited the exemption — correct for `/status`,
/// wrong for a command that spends provider money or holds a subprocess.
/// So they carry their own, here, in the command rather than in the
/// gateway: every surface reaches these through `gen_command` /
/// `tcl_command`, and a brake only the gateway applied would be a brake
/// the REPL and the desktop could walk around.
fn claim_or_refuse(
    ctx: &AclCtx<'_>,
    prefix: &str,
    limit: u64,
    what: &str,
    knob: &str,
) -> Option<String> {
    let Some(p) = ctx.principal else {
        return None; // an owner surface is not on a budget
    };
    let refused = ctx
        .store
        .lock()
        .ok()
        .and_then(|s| s.claim_rate(prefix, &p.subject, limit, 60 * 60).ok())
        .flatten()?;
    Some(format!(
        "[veles] {} has started {limit} {what} in the last hour, which is the ceiling for {}. \
         Try again in {refused}s — an admin can raise it with `/limits {knob} {} <n>`.",
        p.subject,
        p.role.as_str(),
        p.role.as_str()
    ))
}

/// The audio door, wired to the SAME cap the tool answers to.
///
/// `text_to_speech` is `Cap::Tts` (`acl::tool_cap`), but `/gen audio`
/// called `TtsTool` directly and asked only for `media-gen` — so for a
/// while a `user` was refused the tool and allowed the command. `Tts` is
/// a user cap now (it is cheap), which means this gate refuses nobody
/// who could already speak; the point is that the two doors read one
/// table, so the next change to `Tts`'s tier lands on both.
pub(crate) fn audio_gate(principal: Option<&veles_core::acl::Principal>) -> Option<String> {
    if caller_is(principal, veles_core::acl::Cap::Tts) {
        None
    } else {
        Some(denied(principal, "`/gen audio`", veles_core::acl::Cap::Tts))
    }
}

/// The video escalation: rendering a clip is an order of magnitude
/// pricier than a picture — one 5-second job cost 1200 credits on the
/// live plan — so `/gen video` needs `video-gen` (trusted+) even
/// though the `/gen` floor only asks for `media-gen`. Separate from
/// the tool-layer mapping by necessity, and pinned to it by a test.
pub(crate) fn video_gate(principal: Option<&veles_core::acl::Principal>) -> Option<String> {
    if caller_is(principal, veles_core::acl::Cap::VideoGen) {
        None
    } else {
        Some(denied(
            principal,
            "`/gen video` (a clip costs a thousand credits)",
            veles_core::acl::Cap::VideoGen,
        ))
    }
}

/// `/models [provider] [text]` — what a provider serves.
///
/// USER, 2026-10-02: "the intent was for /models to list them" (it had
/// reached the model as chat from IRC), "use current provider if no
/// provider specified", and "do not list 200 openrouter models". So:
///
///   - no provider named → the SESSION's provider (its first route), so
///     a chat on a personal plan is shown that plan, not the default;
///   - the configured models are listed; a live catalog's extras
///     (discovery, when the provider wants it) are COUNTED, never
///     dumped — `text` searches them, capped at `MODELS_SHOWN`;
///   - a first word that names no provider is search text for the
///     session's provider, and a search that finds nothing names the
///     providers, so a misspelt provider still lands somewhere useful.
///
/// A read: the door is `cmd-status`, like `/model` with no argument.
/// Switching stays `/model <name>`, which checks `cmd-session`.
pub async fn models_command(
    cfg: &veles_core::config::VelesConfig,
    current: (&str, &str),
    rest: &str,
) -> String {
    let (cur_provider, cur_model) = current;
    // A route may spell its provider `custom:<name>` (`VelesConfig::provider`).
    let cur_provider = cur_provider.strip_prefix("custom:").unwrap_or(cur_provider);
    let mut words = rest.split_whitespace();
    let named = words.clone().next().and_then(|w| {
        cfg.custom_providers
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(w))
    });
    let (entry, text) = match named {
        Some(e) => {
            words.next();
            (Some(e), words.collect::<Vec<_>>().join(" "))
        }
        None => (cfg.provider(cur_provider), rest.trim().to_string()),
    };
    let providers = || {
        cfg.custom_providers
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let Some(entry) = entry else {
        return format!(
            "this session's provider ({}) is not one of the configured ones.\n\
             providers: {} · /models <provider>",
            if cur_provider.is_empty() {
                "none"
            } else {
                cur_provider
            },
            providers()
        );
    };

    let discovered =
        veles_core::discovery::models_for(entry, &veles_core::config::config_dir()).await;
    let merged = veles_core::discovery::merge(entry, &discovered);
    let mark = |id: &str| {
        if entry.name == cur_provider && id == cur_model {
            format!("{id} (this session)")
        } else {
            id.to_string()
        }
    };
    let whose = if entry.name == cur_provider {
        format!("{} (this session's provider)", entry.name)
    } else {
        entry.name.clone()
    };
    let mut out = Vec::new();

    if !text.is_empty() {
        let needle = text.to_lowercase();
        let hits: Vec<&str> = merged
            .iter()
            .map(|m| m.id.as_str())
            .filter(|id| id.to_lowercase().contains(&needle))
            .collect();
        if hits.is_empty() {
            out.push(format!("{whose}: nothing matching \"{text}\"."));
            out.push(format!("providers: {} · /models <provider>", providers()));
            return out.join("\n");
        }
        out.push(format!("{whose}: {} matching \"{text}\"", hits.len()));
        out.push(listing(&hits, &mark));
        if hits.len() > MODELS_SHOWN {
            out.push(format!(
                "+{} more — narrow the text",
                hits.len() - MODELS_SHOWN
            ));
        }
    } else {
        let curated = entry.models.model_names();
        // A provider with no list of its own is all catalog: its head is
        // shown and the rest counted, like any other overflow.
        let all_catalog = curated.is_empty();
        let ids: Vec<&str> = merged
            .iter()
            .map(|m| m.id.as_str())
            .filter(|id| all_catalog || curated.iter().any(|c| c == id))
            .collect();
        // Everything not printed: the live catalog's extras, and any
        // list longer than MODELS_SHOWN. (`merge` keeps every curated
        // name, so `ids` is a subset of `merged`.)
        let unlisted = merged.len() - ids.len().min(MODELS_SHOWN);
        out.push(format!(
            "{whose}: {} {}",
            ids.len(),
            if all_catalog { "live" } else { "configured" }
        ));
        out.push(listing(&ids, &mark));
        if unlisted > 0 {
            out.push(format!(
                "+{unlisted} more — /models {} <text> searches them",
                entry.name
            ));
        }
    }
    if veles_core::discovery::wanted(entry) && discovered.is_empty() {
        out.push("(its live catalog did not answer: configured models only)".into());
    }
    let others: Vec<&str> = cfg
        .custom_providers
        .iter()
        .map(|p| p.name.as_str())
        .filter(|n| *n != entry.name)
        .collect();
    if !others.is_empty() {
        out.push(format!(
            "others: {} · /models <provider>",
            others.join(", ")
        ));
    }
    out.push("switch with /model <model>@<provider>".into());
    out.join("\n")
}

/// The most `/models` names in one answer: a router's catalog is
/// hundreds long, and an IRC channel is not the place for it.
const MODELS_SHOWN: usize = 40;

/// The first `MODELS_SHOWN` ids, comma-separated: a one-per-line list of
/// twenty-five ids is twenty-five IRC lines.
fn listing(ids: &[&str], mark: &dyn Fn(&str) -> String) -> String {
    ids.iter()
        .take(MODELS_SHOWN)
        .map(|id| mark(id))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `/quota` — what the UPSTREAM plan has left.
///
/// Deliberately not a subcommand of `/usage`. `/usage` reports the
/// ledger this agent writes (`store.usage_totals`); this reports what
/// Alibaba says remains, which is the number that actually stops a turn.
/// Two sources, two truths, two failure modes.
///
/// THE TIER IS A LINE THROUGH THE CONTENT, not a switch on the name.
/// `builtin_cap` puts the door at `cmd-status`, because how much of the
/// plan is left is exactly what a caller needs in order to know whether
/// to ask for something — refusing it only means they find out by being
/// refused mid-turn. What is NOT open is the account behind the agent:
/// seat and subscription ids, who else is on the org, and the month's
/// spend. Those need `cmd-admin`, checked here, the way `/acl` keeps its
/// self-report open and gates its rule listing.
///
/// Every verb reads a cache. Only `refresh` goes to Singapore, and the
/// answer always says how old the figure it printed is.
pub async fn quota_command(
    ctx: &AclCtx<'_>,
    cfg: &veles_core::config::VelesConfig,
    rest: &str,
    active_provider: Option<&str>,
) -> String {
    use veles_core::acl::Cap;

    if !cfg.tokenplan.enabled {
        return "[tokenplan] disabled — set `[tokenplan] enabled = true` in config.toml \
                (and an AccessKey pair in .env) to read the plan's quota."
            .to_string();
    }

    let mut words = rest.split_whitespace();
    let verb = words.next().unwrap_or("").to_ascii_lowercase();
    let arg = words.next();

    // Everything past the bare form names the account, so it takes the
    // higher tier — including `refresh`, which SPENDS a remote call and
    // is therefore a write in the only sense that matters here.
    let needs_admin = !verb.is_empty();
    if needs_admin && !caller_is(ctx.principal, Cap::CmdAdmin) {
        return denied(ctx.principal, &format!("/quota {verb}"), Cap::CmdAdmin);
    }
    let detail = needs_admin;

    let ttl = std::time::Duration::from_secs(cfg.tokenplan.cache_ttl_secs);

    match verb.as_str() {
        "" | "team" | "personal" | "zai" | "z.ai" | "kimi" => {
            let (snap, age) = veles_core::tokenplan::snapshot(cfg, false, None).await;
            // The marker, not the slice: an explicit `/quota team` from
            // a personal session still answers the team section — the
            // caller asked in their own words — but the DEFAULT form
            // says which pool the session actually draws from.
            let full = veles_core::tokenplan::format_quota_report_for(
                &snap,
                age,
                ttl,
                detail,
                active_provider,
            );
            match verb.as_str() {
                "team" => section_of(&full, "team"),
                "personal" => section_of(&full, "personal"),
                "zai" | "z.ai" => section_of(&full, "zai"),
                "kimi" => section_of(&full, "kimi"),
                _ => full,
            }
        }
        "refresh" => {
            let snap = veles_core::tokenplan::refresh(cfg, None).await;
            veles_core::tokenplan::format_quota_report_for(
                &snap,
                std::time::Duration::from_secs(0),
                ttl,
                detail,
                active_provider,
            )
        }
        // `/quota spend 2026-08` — a month other than this one needs a
        // fresh read, since the cache holds only the current month.
        "spend" => {
            let month = arg.map(str::to_string);
            if let Some(m) = &month {
                if !month_is_well_formed(m) {
                    return format!("/quota spend: {m:?} is not a YYYY-MM month");
                }
            }
            let snap = veles_core::tokenplan::refresh(cfg, month.as_deref()).await;
            match (&snap.team, &snap.team_error) {
                (Some(t), _) => match &t.spend {
                    Some(sp) => format!(
                        "spend {} · {} {} ({} pretax + {} tax)",
                        sp.bill_month, sp.currency, sp.total, sp.pretax, sp.tax
                    ),
                    None => format!(
                        "spend {}: not reported",
                        month.unwrap_or_else(|| "this month".into())
                    ),
                },
                (None, Some(e)) => format!("spend: unavailable — {e}"),
                (None, None) => "spend: not configured".to_string(),
            }
        }
        other => format!(
            "/quota: unknown verb {other:?} — try: /quota, /quota team, \
             /quota personal, /quota zai, /quota kimi, /quota spend [YYYY-MM], \
             /quota refresh"
        ),
    }
}

/// Pull one labelled section out of the rendered report, keeping the
/// header line so the answer still says how old the reading is.
///
/// The report is rendered ONCE, by the one formatter, and sliced here —
/// rather than given a second code path per verb, which is how the
/// surfaces drifted apart before `help_entries` existed. Section heads
/// may carry the `← active` marker, which is not part of the name.
fn section_of(report: &str, name: &str) -> String {
    let mut out = Vec::new();
    let mut taking = false;
    for line in report.lines() {
        if line.starts_with("plan quota") || line.starts_with("alibaba token plan") {
            out.push(line);
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') && !line.starts_with('(') {
            let head = line.trim();
            taking = head == name || head == format!("{name} ← active");
            if taking {
                out.push(line);
            }
            continue;
        }
        if taking {
            out.push(line);
        }
    }
    if out.len() <= 1 {
        return format!("{name}: nothing reported");
    }
    out.join("\n")
}

/// `YYYY-MM`, which is what the billing API's `billMonth` takes.
fn month_is_well_formed(m: &str) -> bool {
    let Some((y, mo)) = m.split_once('-') else {
        return false;
    };
    y.len() == 4
        && y.chars().all(|c| c.is_ascii_digit())
        && mo.len() == 2
        && mo.chars().all(|c| c.is_ascii_digit())
        && (1..=12).contains(&mo.parse::<u32>().unwrap_or(0))
}

/// `/gen` — one generation, one reply, no turn. Async because the
/// generation itself is HTTP; the gateway awaits it
/// (`GATEWAY_OVERRIDES`) and the owner surfaces block on a fresh
/// runtime, exactly like `/sandbox`.
/// `/configsync [apply|drop]` — the config changes peers sent that wait here.
///
/// With `[cluster] configsync_auto` a peer's offer applies on receipt,
/// except rows for a security section (`acl`, `approvals`, `sandbox`,
/// `hooks`, `plugins`, `mcp_servers`) and offers with a conflict, which
/// stay in the inbox. This lists them; `apply` lands every one (the
/// conflicts' winners included), as `veles configsync apply --inbox`
/// does, and `drop` sets them aside unapplied. OWNER ONLY: it is the
/// decision the hold leaves to the owner.
pub fn configsync_command(ctx: &AclCtx<'_>, rest: &str) -> String {
    if !caller_is(ctx.principal, veles_core::acl::Cap::All) {
        return denied(ctx.principal, "/configsync", veles_core::acl::Cap::All);
    }
    let waiting = match veles_core::configsync::inbox_waiting() {
        Ok(w) => w,
        Err(e) => return format!("reading the config sync inbox failed: {e:#}"),
    };
    match rest.trim() {
        "" => {
            if waiting.is_empty() {
                return "no config changes wait here".into();
            }
            let mut out = format!("{} offer(s) wait for you:\n", waiting.len());
            for (from, keys) in &waiting {
                out.push_str(&format!("  from {from}: {}\n", keys.join(", ")));
            }
            out.push_str(
                "land them all: /configsync apply · discard them: /configsync drop \
                 (a value that differs per node belongs in [node.overrides])",
            );
            out
        }
        "apply" => {
            if waiting.is_empty() {
                return "no config changes wait here".into();
            }
            let me = match veles_core::node::NodeIdentity::load_or_create(
                &veles_core::config::config_dir(),
            ) {
                Ok(i) => i.id,
                Err(e) => return format!("this node has no identity: {e:#}"),
            };
            let dir = veles_core::configsync::inbox_dir();
            let store = ctx.store.lock_ok();
            let mut out = Vec::new();
            let pending = match veles_core::configsync::inbox_pending(&dir) {
                Ok(p) => p,
                Err(e) => return format!("reading the inbox failed: {e:#}"),
            };
            for p in pending {
                match veles_core::configsync::apply_inbox_file(&store, &me, &p) {
                    Ok(done) => {
                        out.push(format!(
                            "— from {} —\n{}",
                            done.from,
                            done.report.summarize()
                        ));
                        if !done.archived {
                            out.push("(applied, but the offer could not be archived)".into());
                        }
                    }
                    Err(e) => out.push(format!("{}: not applied: {e:#}", p.display())),
                }
            }
            out.join("\n")
        }
        // A held change that is wrong HERE — a peer's [sandbox] for its
        // own runtime, say — would otherwise wait, and nag, for ever.
        "drop" => {
            let dir = veles_core::configsync::inbox_dir();
            let gone = dir.join("dropped");
            let pending = match veles_core::configsync::inbox_pending(&dir) {
                Ok(p) => p,
                Err(e) => return format!("reading the inbox failed: {e:#}"),
            };
            let n = pending
                .iter()
                .filter(|p| veles_core::configsync::drop_inbox_file(p).is_ok())
                .count();
            format!(
                "dropped {n} offer(s) — their changes are not applied here (kept in {})",
                gone.display()
            )
        }
        other => format!("unknown: /configsync {other}\nusage: /configsync [apply|drop]"),
    }
}

/// `/defaults` — read and write the routes every NEW session starts on.
///
/// `/model` is session-scoped and says so. Until now the only way to
/// change what the next session starts with was the desktop's
/// `POST /api/config/set` or an editor, which meant the one knob the
/// operator reaches for most — "ali-personal is out of quota, put
/// everything on ali-team" — was the one knob with no chat surface.
///
/// Five tables answer one question ("what am I spending on"), so the
/// no-argument form prints them together: `[model]`, `[vision]`,
/// `[imagegen]`, `[videogen]`, `[asr]`, and the four loose `agent.tts_*`
/// fields.
///
/// OWNER ONLY. This writes a file on disk and changes what every other
/// caller's turns cost.
pub fn defaults_command(ctx: &AclCtx<'_>, rest: &str) -> String {
    if !caller_is(ctx.principal, veles_core::acl::Cap::All) {
        return denied(ctx.principal, "/defaults", veles_core::acl::Cap::All);
    }
    let cfg = match veles_core::config::load() {
        Ok(c) => c,
        Err(e) => return format!("config load: {e}"),
    };
    let mut words = rest.split_whitespace();
    let Some(sub) = words.next() else {
        return defaults_listing(&cfg);
    };
    let args: Vec<&str> = words.collect();

    // `model` and `provider` are the two halves of `[model]`. The first
    // is NOT parsed here either — it is handed to `agent::resolve_route`,
    // which is `/model`'s own resolver with the session side removed, so
    // `model@provider`, `provider/model`, aliases and `custom:` mean the
    // same thing in both commands and there is one parser for them.
    match sub {
        "model" | "default" => {
            let Some(v) = args.first() else {
                return "usage: /defaults model <name|model@provider|provider/model>".into();
            };
            defaults_model(&cfg, v).unwrap_or_else(|e| e)
        }
        "provider" => {
            let Some(v) = args.first() else {
                return "usage: /defaults provider <name>".into();
            };
            // Refuse a provider that is not configured, by name, and say
            // what IS — the same shape `imagegen::resolve_endpoint` uses.
            // Writing an unknown one here would not fail until the next
            // session's first turn.
            if cfg.provider(v).is_none() {
                let known: Vec<&str> = cfg
                    .custom_providers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect();
                return format!(
                    "provider '{v}' is not configured — known: {}",
                    known.join(", ")
                );
            }
            write_default("model.provider", &["model", "provider"], v)
        }
        // Per-class routes: which model a NEW session starts on, for a
        // class of caller. Stored as runtime rows so they are editable
        // from a chat rather than only from a file — the same
        // config-plus-store duality `/sandbox bind` has.
        "binds" | "bind" | "unbind" => defaults_binds(ctx, sub, &args),
        "switch" => defaults_switch(&cfg, &args),
        "reasoning" => {
            let Some(v) = args.first() else {
                return "usage: /defaults reasoning <none|low|medium|high|xhigh>".into();
            };
            if !["none", "minimal", "low", "medium", "high", "xhigh"].contains(v) {
                return format!(
                    "'{v}' is not an effort — none, minimal, low, medium, high or xhigh"
                );
            }
            write_default("agent.reasoning_effort", &["agent", "reasoning_effort"], v)
        }
        "vision" | "image" | "imagegen" | "video" | "videogen" | "asr" | "tts" | "audio" => {
            let section = match sub {
                "vision" => "vision",
                "image" | "imagegen" => "imagegen",
                "video" | "videogen" => "videogen",
                "asr" => "asr",
                _ => "tts",
            };
            let Some(provider) = args.first() else {
                return format!("usage: /defaults {sub} <provider> [<model>]");
            };
            if cfg.provider(provider).is_none() && !provider.eq_ignore_ascii_case("comfyui") {
                let known: Vec<&str> = cfg
                    .custom_providers
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect();
                return format!(
                    "provider '{provider}' is not configured — known: {}",
                    known.join(", ")
                );
            }
            // TTS is four loose fields on [agent] rather than a
            // GenSection, so its paths are spelled differently. That
            // asymmetry is in the config, not invented here.
            let (ppath, mpath, hint): (Vec<&str>, Vec<&str>, String) = if section == "tts" {
                (
                    vec!["agent", "tts_provider"],
                    vec!["agent", "tts_model"],
                    "agent.tts_provider".to_string(),
                )
            } else {
                (
                    vec![section, "provider"],
                    vec![section, "model"],
                    format!("{section}.provider"),
                )
            };
            let model = args.get(1).copied();
            let report = veles_core::configedit::edit(&hint, |d| {
                veles_core::configedit::set_value(d, &ppath, (*provider).into())?;
                if let Some(m) = model {
                    veles_core::configedit::set_value(d, &mpath, m.into())?;
                }
                Ok(())
            });
            match report {
                Ok(r) => {
                    // Report the KEYS that changed, not a friendly label.
                    // `/defaults image …` writes `[imagegen]`, and
                    // `/defaults tts …` writes `agent.tts_*` — naming the
                    // real path is how the reply stays checkable against
                    // the file, and it matches `write_default`'s shape.
                    let mut lines = vec![format!("{} = {provider}", ppath.join("."))];
                    if let Some(m) = model {
                        lines.push(format!("{} = {m}", mpath.join(".")));
                    }
                    format!("{}\n({})", lines.join("\n"), r.effect.note())
                }
                Err(e) => format!("could not write config.toml: {e:#}"),
            }
        }
        other => format!(
            "unknown: /defaults {other}\n{}",
            "subcommands: model, provider, reasoning, vision, image, video, asr, tts, \
             switch <from> <to>, binds/bind/unbind — or no argument to read them all"
        ),
    }
}

/// `/defaults binds | bind <k=v>… | unbind <k=v>…`
///
/// Runtime rows in the store, not config edits, and that is the same
/// choice `/sandbox bind` made: a rule about who gets which model is
/// something you adjust while watching a channel, and a config write
/// per adjustment would fight whatever the operator has in the file.
/// `model_binds::binds` reads config rules first and store rows after,
/// so a declared rule still wins a tie.
fn defaults_binds(ctx: &AclCtx<'_>, sub: &str, args: &[&str]) -> String {
    let cfg = match veles_core::config::load() {
        Ok(c) => c,
        Err(e) => return format!("config load: {e}"),
    };
    if sub == "binds" {
        return veles_core::model_binds::listing(&cfg, Some(ctx.store));
    }

    // `k=v` pairs, so the selectors are named rather than positional —
    // five optional selectors and three optional route fields have no
    // readable positional order.
    let mut b = veles_core::config::ModelBind::default();
    let mut unknown: Vec<&str> = Vec::new();
    for a in args {
        let Some((k, v)) = a.split_once('=') else {
            unknown.push(a);
            continue;
        };
        let v = v.trim().to_string();
        let slot = match k.trim().to_ascii_lowercase().as_str() {
            "identity" | "who" => &mut b.identity,
            "network" | "net" => &mut b.network,
            "channel" | "chan" => &mut b.channel,
            "platform" => &mut b.platform,
            "role" => &mut b.role,
            "provider" => &mut b.provider,
            "model" => &mut b.model,
            "reasoning" => &mut b.reasoning,
            _ => {
                unknown.push(a);
                continue;
            }
        };
        *slot = Some(v);
    }
    if !unknown.is_empty() {
        return format!(
            "not a selector: {}\nusage: /defaults bind [identity=] [network=] [channel=] \
             [platform=] [role=] model= provider= [reasoning=]\n\
             role compares: role=<=trusted is everyone at or below trusted",
            unknown.join(" ")
        );
    }

    if sub == "unbind" {
        let key = format!(
            "{}{}",
            veles_core::model_binds::BIND_PREFIX,
            veles_core::model_binds::bind_key(&b)
        );
        let gone = ctx.store.lock_ok().meta_delete(&key).unwrap_or(false);
        return if gone {
            "unbound (a rule in config.toml, if any, is untouched)".into()
        } else {
            "no runtime bind with those selectors".into()
        };
    }

    // bind
    if b.provider.is_none() && b.model.is_none() {
        return "a bind needs a route: model=<name> and/or provider=<name>".into();
    }
    if let Some(r) = b.role.as_deref() {
        if !veles_core::selector::role_spec_is_valid(r) {
            return format!(
                "'{r}' is not a role — banned, guest, user, trusted, admin or owner, \
                 optionally with <=, >=, < or >"
            );
        }
    }
    if let Some(p) = b.provider.as_deref() {
        if cfg.provider(p).is_none() {
            let known: Vec<&str> = cfg
                .custom_providers
                .iter()
                .map(|x| x.name.as_str())
                .collect();
            return format!(
                "provider '{p}' is not configured — known: {}",
                known.join(", ")
            );
        }
    }
    let key = format!(
        "{}{}",
        veles_core::model_binds::BIND_PREFIX,
        veles_core::model_binds::bind_key(&b)
    );
    let json = match serde_json::to_string(&b) {
        Ok(j) => j,
        Err(e) => return format!("could not encode the bind: {e}"),
    };
    // Keyed on the SELECTORS, so re-binding the same ones replaces
    // rather than accumulating a second rule nobody can see the effect
    // of.
    match ctx.store.lock_ok().meta_set(&key, &json) {
        Ok(()) => format!(
            "bound → {}\n(new sessions matching it; open ones keep their route)",
            veles_core::model_binds::model_arg(&b).unwrap_or_default()
        ),
        Err(e) => format!("could not store the bind: {e:#}"),
    }
}

/// `/defaults model <name|model@provider|provider/model|alias>`
///
/// RESOLVES the spelling, then writes the two keys `[model]` actually
/// reads. It used to write the argument into `model.default` verbatim,
/// on the reasoning that `/model`'s resolver would pick it up later —
/// but nothing ever resolves `[model] default`. `build_routes` takes the
/// provider from `[model] provider` and sends `default` as a bare model
/// id, so `/defaults model ali-personal/qwen3.8-flash` asked the
/// already-configured provider for a model literally named
/// `ali-personal/qwen3.8-flash`: an instant 404 that reads like the
/// provider rejecting a model it was never asked about, from a command
/// whose own usage line advertised that spelling.
///
/// The resolver is `agent::resolve_route` — `/model`'s own, with the
/// session side removed — so the two commands cannot drift on what
/// `openrouter/free` means. That matters most for the slashed model ids:
/// `resolve_route` claims one at step 3 before step 3.5 can split it, so
/// a real model id survives whole and gains the provider that lists it.
///
/// Both keys land in ONE `configedit::edit`, so a failure partway cannot
/// leave `[model]` naming a provider and a model that were never a pair.
/// `/defaults model`'s writer, public so `veles model` and the desktop
/// set the default through the same resolution and the same one write.
pub fn defaults_model(cfg: &veles_core::config::VelesConfig, spec: &str) -> Result<String, String> {
    let known = || -> String {
        cfg.custom_providers
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let r = match veles_core::agent::resolve_route(cfg, spec, cfg.model.provider.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            return Err(format!(
                "could not resolve '{spec}': {e:#}\nknown providers: {}",
                known()
            ))
        }
    };
    let Some(model) = r.model.clone() else {
        // `custom:<provider>` on a provider that declares no default
        // model of its own. There is nothing to put in `model.default`,
        // and writing only the provider half would pair it with whatever
        // model was there before — a route nobody asked for.
        return Err(format!(
            "'{spec}' switches to provider {}, which declares no default model — \
             name one: /defaults model <model>@{}",
            r.shown_provider, r.shown_provider
        ));
    };

    // The provider half moves only when the resolution NAMED one. The
    // last-resort arm accepts an unlisted model id as a guess and
    // deliberately leaves the provider alone, so a bare name cannot
    // silently re-point the whole install at a different account.
    let provider = r.provider.clone();
    let report = veles_core::configedit::edit("model.default", |d| {
        if let Some(p) = &provider {
            veles_core::configedit::set_value(d, &["model", "provider"], p.as_str().into())?;
        }
        veles_core::configedit::set_value(d, &["model", "default"], model.as_str().into())
    });
    match report {
        Ok(rep) => {
            // The same line `/model` prints, so the two commands report
            // a resolution identically, then the KEYS that changed —
            // naming the real path is what keeps the reply checkable
            // against the file.
            let mut wrote = vec![format!("model.default = {model}")];
            if let Some(p) = &provider {
                // Printed second but written first: `model.provider` is
                // the half that decides which account is billed.
                wrote.insert(0, format!("model.provider = {p}"));
            }
            Ok(format!(
                "{}\n{}\n({})",
                veles_core::agent::Agent::resolution_msg(
                    spec,
                    &r.shown_provider,
                    &r.shown_model,
                    &r.how
                ),
                wrote.join("\n"),
                rep.effect.note()
            ))
        }
        Err(e) => Err(format!("could not write config.toml: {e:#}")),
    }
}

/// One `[section] key = value` write, reported with what it actually
/// affects.
fn write_default(hint: &str, path: &[&str], value: &str) -> String {
    match veles_core::configedit::edit(hint, |d| {
        veles_core::configedit::set_value(d, path, value.into())
    }) {
        // `effect.note()` and not "ok": `model.*` is NewSessions, so the
        // honest answer is that the session you typed this in keeps its
        // old route. Saying "ok" is how someone concludes the knob is
        // broken.
        Ok(r) => format!("{hint} = {value}\n({})", r.effect.note()),
        Err(e) => format!("could not write config.toml: {e:#}"),
    }
}

/// Every default route, in one block.
fn defaults_listing(cfg: &veles_core::config::VelesConfig) -> String {
    let or = |o: &Option<String>| o.clone().unwrap_or_else(|| "(unset)".into());
    let gen = |label: &str, g: &veles_core::config::GenSection| {
        format!(
            "  {label:<9} {} / {}{}",
            or(&g.provider),
            or(&g.model),
            match &g.api {
                Some(a) if !a.is_empty() => format!("  [api: {a}]"),
                _ => String::new(),
            }
        )
    };
    let mut out = String::from("defaults for NEW sessions (config.toml):\n");
    out.push_str(&format!(
        "  {:<9} {} / {}\n",
        "chat",
        or(&cfg.model.provider),
        or(&cfg.model.default)
    ));
    out.push_str(&format!(
        "  {:<9} {} / {}\n",
        "vision",
        or(&cfg.vision.provider),
        or(&cfg.vision.model)
    ));
    out.push_str(&gen("image", &cfg.imagegen));
    out.push('\n');
    out.push_str(&gen("video", &cfg.videogen));
    out.push('\n');
    out.push_str(&gen("asr", &cfg.asr));
    out.push('\n');
    out.push_str(&format!(
        "  {:<9} {} / {}{}\n",
        "tts",
        or(&cfg.agent.tts_provider),
        or(&cfg.agent.tts_model),
        match &cfg.agent.tts_voice {
            Some(v) if !v.is_empty() => format!("  [voice: {v}]"),
            _ => String::new(),
        }
    ));
    // What each unset route MEANS — the tables do not agree, and a
    // listing that only says "(unset)" leaves the reader to find out.
    out.push_str(
        "  (unset: vision and asr are off · image and video use local ComfyUI · \
         tts uses the chat provider)\n",
    );
    out.push_str(&format!(
        "  {:<9} {}  (a provider's own `reasoning_effort` wins unless the session set one; \
         \"off\" always wins)\n",
        "reasoning",
        or(&cfg.agent.reasoning_effort),
    ));
    out.push_str(&format!(
        "  {:<9} {}\n",
        "screen",
        match (&cfg.approvals.screen_provider, &cfg.approvals.screen_model) {
            (None, None) => "approval screening runs on the session's route".to_string(),
            (p, m) => format!("{} / {}", or(p), or(m)),
        }
    ));
    out.push_str(
        "  titles, dream and delegate children run on the chat default; \
         compaction on the session's own route.\n",
    );
    if !cfg.command_aliases.is_empty() {
        out.push_str("\ncommand aliases:\n");
        for a in &cfg.command_aliases {
            out.push_str(&format!(
                "  /{:<16} {} / {} ({})\n",
                a.name,
                a.provider,
                a.model,
                a.kind.as_deref().unwrap_or("chat")
            ));
        }
    }
    if !cfg.fallback_providers.is_empty() {
        let chain: Vec<String> = cfg
            .fallback_providers
            .iter()
            .map(|f| match &f.model {
                Some(m) => format!("{}/{m}", f.provider),
                None => f.provider.clone(),
            })
            .collect();
        out.push_str(&format!("\nfallbacks: {}", chain.join(" → ")));
        if cfg.failover_rules.is_empty() {
            out.push_str(
                "\n  inert: no [[failover]] rule, so the chain is never walked \
                 (a session opts in with /failover)",
            );
        }
        out.push('\n');
    }
    out.push_str(
        "\nset with /defaults <model|provider|reasoning|vision|image|video|asr|tts> …\n\
         /defaults switch <from> <to> moves every route on one provider to another;\n\
         /defaults binds lists per-caller routes; /model changes THIS session only.",
    );
    out
}

/// `/defaults switch <from> <to> [apply]` — every default route on
/// provider `from` (chat, vision, image, video, asr, tts, the approval
/// screen, command aliases) moved to `to` in ONE write. A preview
/// first; `apply` writes it.
///
/// USER 2026-10-04: "I told a model to update defaults and it spent 2
/// hours on it." The change was eight `provider` lines, team → personal,
/// spread over six tables; the time went on finding them. Model
/// aliases and the fallback chain are NOT moved: an alias names one
/// route on purpose, and the chain exists to be somewhere else.
fn defaults_switch(cfg: &veles_core::config::VelesConfig, args: &[&str]) -> String {
    let (Some(from), Some(to)) = (args.first().copied(), args.get(1).copied()) else {
        return "usage: /defaults switch <from-provider> <to-provider> [apply]".into();
    };
    let apply = args.get(2).copied() == Some("apply");
    let Some(target) = cfg.provider(to) else {
        let known: Vec<&str> = cfg
            .custom_providers
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        return format!(
            "provider '{to}' is not configured — known: {}",
            known.join(", ")
        );
    };
    // What `to` serves, when it says: a list, a map's keys, its own
    // default. None = it does not say, and nothing is flagged.
    let served: Option<Vec<String>> = {
        let mut v: Vec<String> = match &target.models {
            veles_core::config::ModelsSpec::List(l) => l.clone(),
            veles_core::config::ModelsSpec::Map(m) => m.keys().cloned().collect(),
            veles_core::config::ModelsSpec::Empty => Vec::new(),
        };
        v.extend(target.model.clone());
        (!v.is_empty()).then_some(v)
    };
    let on = |p: Option<&str>| p.map(|p| p.strip_prefix("custom:").unwrap_or(p)) == Some(from);
    // (label, toml path, the model to check against `to`'s list). A
    // provider's `models` names its CHAT models, so only chat-shaped
    // routes are checked; an image or speech model is never listed there.
    let mut hits: Vec<(String, Vec<String>, Option<String>)> = Vec::new();
    let mut add = |label: &str, path: &[&str], model: Option<String>| {
        hits.push((
            label.to_string(),
            path.iter().map(|s| s.to_string()).collect(),
            model,
        ))
    };
    if on(cfg.model.provider.as_deref()) {
        add("chat", &["model", "provider"], cfg.model.default.clone());
    }
    if on(cfg.vision.provider.as_deref()) {
        add("vision", &["vision", "provider"], cfg.vision.model.clone());
    }
    for (label, sect, g) in [
        ("image", "imagegen", &cfg.imagegen),
        ("video", "videogen", &cfg.videogen),
        ("asr", "asr", &cfg.asr),
    ] {
        if on(g.provider.as_deref()) {
            add(label, &[sect, "provider"], None);
        }
    }
    if on(cfg.agent.tts_provider.as_deref()) {
        add("tts", &["agent", "tts_provider"], None);
    }
    if on(cfg.approvals.screen_provider.as_deref()) {
        add(
            "approval screen",
            &["approvals", "screen_provider"],
            cfg.approvals.screen_model.clone(),
        );
    }
    for a in &cfg.command_aliases {
        if on(Some(a.provider.as_str())) {
            add(
                &format!("/{}", a.name),
                &["command_aliases", &a.name],
                matches!(a.kind.as_deref(), None | Some("chat")).then(|| a.model.clone()),
            );
        }
    }
    if hits.is_empty() {
        return format!("no default route is on {from} — nothing to switch");
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|(label, path, model)| {
            let warn = match (model, &served) {
                (Some(m), Some(list)) if !list.iter().any(|s| s == m) => {
                    format!("  ⚠ {m} is not in {to}'s model list")
                }
                _ => String::new(),
            };
            let key = if path[0] == "command_aliases" {
                format!("[[command_aliases]] {}", path[1])
            } else {
                path.join(".")
            };
            format!("  {label:<16} {key}: {from} → {to}{warn}")
        })
        .collect();
    if !apply {
        return format!(
            "{} route(s) on {from}:\n{}\n\nwrite them: /defaults switch {from} {to} apply",
            hits.len(),
            lines.join("\n")
        );
    }
    let report = veles_core::configedit::edit("model.provider", |d| {
        for (_, path, _) in &hits {
            if path[0] == "command_aliases" {
                if let Some(arr) = d
                    .get_mut("command_aliases")
                    .and_then(|i| i.as_array_of_tables_mut())
                {
                    for t in arr.iter_mut() {
                        if t.get("name").and_then(|v| v.as_str()) == Some(path[1].as_str()) {
                            t["provider"] = toml_edit::value(to);
                        }
                    }
                }
            } else {
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                veles_core::configedit::set_value(d, &p, to.into())?;
            }
        }
        Ok(())
    });
    match report {
        Ok(r) => format!("{}\n({})", lines.join("\n"), r.effect.note()),
        Err(e) => format!("could not write config.toml: {e:#}"),
    }
}

pub async fn gen_command(ctx: &AclCtx<'_>, rest: &str) -> String {
    if let Some(d) = gen_denied(ctx.principal) {
        return d;
    }
    let Some(req) = parse_gen(rest) else {
        return "usage: /gen <image|video|audio> [route] [flags] <prompt…>\n\
                routes: comfyui (local), or a configured provider — `provider/model` as the first word,\n\
                or --provider/--model; `/gen image --list` names what is configured\n\
                flags: --list  --voice V (audio)  --speed F (audio)\n\
                (video needs the `video-gen` cap — trusted+; a clip costs ~1200 credits)"
            .into();
    };
    gen_run(ctx, req).await
}

/// Everything `/gen` does once the line has become a `GenRequest`.
///
/// Split out so a media command ALIAS (`/qwen-image-3-pro <prompt>`,
/// `veles_core::aliases`) can build the request directly instead of
/// re-serialising itself into `/gen`'s grammar and hoping `parse_gen`
/// reads it back the same way. A prompt whose first word happens to
/// contain a slash would not survive that round trip, and the failure
/// would be a generation billed against the wrong route.
///
/// The caller is responsible for `gen_denied` — the floor — because both
/// entry points need it before they build anything.
pub(crate) async fn gen_run(ctx: &AclCtx<'_>, req: GenRequest) -> String {
    use veles_core::tools::Tool;

    if req.list {
        let cfg = match veles_core::config::load() {
            Ok(c) => c,
            Err(e) => return format!("config load: {e}"),
        };
        let mut out = format!(
            "image routes:\n  {}",
            veles_core::imagegen::route_listing(&cfg)
        );
        out.push_str(&format!(
            "\nvideo routes:\n  {}",
            veles_core::imagegen::gen_listing(
                &cfg,
                &cfg.videogen,
                "[videogen]",
                Some("comfyui (local ComfyUI — needs a workflow; video is real on a dashscope endpoint)"),
            )
        ));
        out.push_str("\naudio (tts) routes: any provider's audio endpoint — shape inferred from its base — or `edge` (edge-tts, free); model/voice from agent.tts_*");
        out.push_str(&format!(
            "\nasr (transcribe_audio) route:\n  {}",
            veles_core::imagegen::gen_listing(&cfg, &cfg.asr, "[asr]", None)
        ));
        if let Some(p) = ctx.principal {
            out.push_str(&format!("\n(calling as {})", p.label()));
        }
        return out;
    }

    if req.prompt.is_empty() && req.kind != GenKind::Audio {
        return "nothing to make — `/gen image <prompt>` (or audio, video)".into();
    }

    // Video is the expensive sibling: same command, higher bar. The
    // floor lets a user reach the branch; `video_gate` (trusted+) is
    // what a 1200-credit clip actually costs. Listing the route needs
    // only the floor — knowing is not spending.
    if req.kind == GenKind::Video {
        if let Some(d) = video_gate(ctx.principal) {
            return d;
        }
    }
    // Audio is the cheap sibling, but it is still a DIFFERENT cap from
    // the floor, and the tool layer has always said so. Checked here so
    // `/gen audio` and `text_to_speech` cannot disagree about who may
    // speak — they did, until 2026-09-11.
    if req.kind == GenKind::Audio {
        if let Some(d) = audio_gate(ctx.principal) {
            return d;
        }
    }

    // ── the rate brakes, after the caps and before any provider call ──
    // Placed here rather than at the top so `--list` (which spends
    // nothing) is never refused: knowing is not spending, the same rule
    // `video_gate` follows.
    let limits = acl_cfg(ctx).limits;
    if let Some(p) = ctx.principal {
        if let Some(d) = claim_or_refuse(
            ctx,
            veles_core::store::Store::MEDIA_RATE_PREFIX,
            limits.media_per_hour_for_subject(p.role, &p.subject, &p.budget_key()),
            "generations",
            "media-per-hour",
        ) {
            return d;
        }
        // Video claims BOTH: it is a generation like any other, and it
        // is the expensive one. Claimed after the media budget so a
        // refusal names the tighter ceiling the caller actually met.
        if req.kind == GenKind::Video {
            if let Some(d) = claim_or_refuse(
                ctx,
                veles_core::store::Store::VIDEO_RATE_PREFIX,
                limits.video_per_hour_for_subject(p.role, &p.subject, &p.budget_key()),
                "clips",
                "video-per-hour",
            ) {
                return d;
            }
        }
    }

    match req.kind {
        GenKind::Image => {
            let mut args = serde_json::json!({"prompt": req.prompt});
            if let Some(p) = &req.provider {
                args["provider"] = p.clone().into();
            }
            if let Some(m) = &req.model {
                args["model"] = m.clone().into();
            }
            veles_core::imagegen::ImageGenTool.call(args).await
        }
        GenKind::Video => {
            // The comfyui route is still a workflow placeholder; a
            // provider route (config or per-call) renders for real on
            // the dashscope async task.
            let mut args = serde_json::json!({"prompt": req.prompt});
            if let Some(p) = &req.provider {
                args["provider"] = p.clone().into();
            }
            if let Some(m) = &req.model {
                args["model"] = m.clone().into();
            }
            veles_core::imagegen::VideoGenTool.call(args).await
        }
        GenKind::Audio => {
            if req.prompt.is_empty() {
                return "nothing to say — `/gen audio <text>` (aliases: tts, voice, say)".into();
            }
            let mut args = serde_json::json!({"text": req.prompt});
            if let Some(p) = &req.provider {
                args["provider"] = p.clone().into();
            }
            if let Some(m) = &req.model {
                args["model"] = m.clone().into();
            }
            if let Some(v) = &req.voice {
                args["voice"] = v.clone().into();
            }
            if let Some(s) = req.speed {
                args["speed"] = s.into();
            }
            // Bound to the CALLER's session, so the tool's own jail sees
            // the same principal the gate above just judged.
            veles_core::tts::TtsTool {
                session_id: ctx.session_id.to_string(),
            }
            .call(args)
            .await
        }
    }
}

/// `/tcl <code>` — evaluate Tcl for the caller and answer with the
/// result. No turn, no model, no provider call.
///
/// The gate is `Cap::Shell` at the NAME (`acl::builtin_cap`), and the
/// interpreter is a second, stricter question: below owner it is the safe
/// interp (no exec/file/socket), and the full `tclsh` goes only to the
/// configured owner — never in dormant mode, where every stranger
/// resolves as owner. That is slopdrop's `tcl` / `tclAdmin` split and it
/// is the split not to flatten.
pub async fn tcl_command(ctx: &AclCtx<'_>, rest: &str) -> String {
    tcl_command_as(ctx, rest, false).await
}

/// Same, with the privileged spelling (`/tcladmin`, `.tcladmin`).
///
/// Which command you typed decides whether the privileged path is
/// ASKED for; your role decides whether you may have it. An owner
/// typing plain `tcl` gets the ordinary interpreter, exactly as they
/// would talking to slopdrop directly — that is the split slopdrop has
/// always had, and collapsing it made `tcl` fail for the owner and
/// nobody else.
pub async fn tcl_command_as(ctx: &AclCtx<'_>, rest: &str, want_admin: bool) -> String {
    tcl_command_as_hashed(ctx, rest, want_admin).await.text
}

/// `tcl_command_as`, plus the content hash of the stored proc the line
/// named (slopdrop's `invoked_hash`). The Bluesky surface's TOS layer
/// keys its per-proc verdict on this; every other caller wants only the
/// text and uses `tcl_command_as`.
pub async fn tcl_command_as_hashed(
    ctx: &AclCtx<'_>,
    rest: &str,
    want_admin: bool,
) -> veles_core::tcleval::TclAnswer {
    let name = if want_admin { "/tcladmin" } else { "/tcl" };
    if !caller_is(ctx.principal, veles_core::acl::Cap::Shell) {
        return (
            denied(ctx.principal, name, veles_core::acl::Cap::Shell),
            None,
        )
            .into();
    }
    if want_admin && !veles_core::tcleval::full_interp_allowed(ctx.principal) {
        return (
            denied(ctx.principal, "`/tcladmin`", veles_core::acl::Cap::All),
            None,
        )
            .into();
    }
    if rest.trim().is_empty() {
        return (
            format!(
                "usage: {name} <code>  — evaluates Tcl and answers with the result (no model turn)"
            ),
            None,
        )
            .into();
    }
    if let Some(p) = ctx.principal {
        if let Some(d) = claim_or_refuse(
            ctx,
            veles_core::store::Store::EVAL_RATE_PREFIX,
            acl_cfg(ctx)
                .limits
                .eval_per_hour_for_subject(p.role, &p.subject, &p.budget_key()),
            "evaluations",
            "eval-per-hour",
        ) {
            return (d, None).into();
        }
    }
    let full = veles_core::tcleval::admin_requested_and_allowed(want_admin, ctx.principal);
    // The caller slopdrop sees (`user`, `$who`, its git author) is the
    // DISPLAY identity — the handle on atproto, the nick on IRC — not the
    // ACL subject. The subject stays upstream for keying/ACL (USER
    // 2026-10-07: "[name] should be the handle, not user:wrath").
    let subject = ctx.principal.map_or("local:owner", |p| p.subject.as_str());
    let caller = veles_core::tcleval::display_caller(ctx.sender, subject);
    veles_core::tcleval::eval_line_hashed(
        rest, full, &caller,
        // The room, for the slopdrop bridge: `[nick]`, `[names]` and
        // `[log]` are read out of interpreter globals that only the bot
        // still holding the connection can fill.
        ctx.sender,
    )
    .await
}

/// `/sandbox` — where this session's tools run, and the management of
/// the named boxes.
///
/// Async because the live half of the answer is podman's: which
/// containers exist, which are running, and purging one. The sync
/// dispatcher blocks on it the way `/compress` does; the gateway
/// awaits it directly (`GATEWAY_OVERRIDES`).
///
/// `where` is open — a caller is entitled to know which box their own
/// files are in, and being told "the shared box for #veles" is what
/// keeps them from assuming they are on the host. Everything that
/// declares, assigns or destroys is `cmd-admin`; `use` is
/// `cmd-session` and additionally checked against the caller's
/// reachable set, so naming a box is never a way into one.
pub async fn sandbox_command(ctx: &AclCtx<'_>, rest: &str) -> String {
    use veles_core::acl::Cap;
    let principal = ctx.principal;
    let store = Some(ctx.store);
    let mut words = rest.split_whitespace();
    let sub = words.next().unwrap_or("");
    let args: Vec<&str> = words.collect();

    match sub {
        "" | "where" | "which" => {
            // No session (the CLI, the IRC `.agent` path): resolve the
            // box the way the gateway would, from the caller and where
            // they are, rather than reporting the owner-surface answer
            // — "the host" — to somebody who will never be on it.
            if ctx.session_id.is_empty() {
                let Some(p) = principal else {
                    return "no session and no caller — nothing to resolve a box from; \
                            `list` shows what exists"
                        .into();
                };
                let sender = ctx.sender.cloned().unwrap_or_default();
                let name = veles_core::sandboxes::resolve_for(p, &sender, store);
                let mut out = format!("you land in the sandbox `{name}`");
                let set = veles_core::sandboxes::reachable(p, &sender, store);
                if p.role >= veles_core::acl::Role::Admin {
                    out.push_str(
                        "\nyou may name any box that exists, and `host: true` reaches the host",
                    );
                } else {
                    out.push_str(&format!("\nyou may name: {}", set.join(", ")));
                }
                return out;
            }
            let session_box = veles_core::sandboxes::session_sandbox(ctx.session_id);
            let zone = veles_core::user_envs::zone_for(
                Some(ctx.session_id),
                veles_core::user_envs::ZoneArg::Unset,
            );
            let mut out = match &zone {
                veles_core::user_envs::Zone::Host => {
                    "your tools run on the HOST — this is an owner surface".to_string()
                }
                veles_core::user_envs::Zone::Sandbox { name, .. } => {
                    format!("your tools run inside the sandbox `{name}`")
                }
            };
            if let Some(b) = &session_box {
                if !matches!(&zone, veles_core::user_envs::Zone::Sandbox { name, .. } if name == b)
                {
                    out.push_str(&format!(" (this session's box is `{b}`)"));
                }
            }
            if let Some(p) = principal {
                let set = veles_core::sandboxes::session_reachable(&p.subject);
                if p.role >= veles_core::acl::Role::Admin {
                    out.push_str(
                        "\nyou may name any box that exists, and `host: true` reaches the host",
                    );
                } else if set.is_empty() {
                    out.push_str("\nyou may name no other box");
                } else {
                    out.push_str(&format!("\nyou may also name: {}", set.join(", ")));
                }
            }
            out
        }

        "list" | "ls" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, "`/sandbox list`", Cap::CmdAdmin);
            }
            let defs = veles_core::sandboxes::defs(store);
            let binds = veles_core::sandboxes::binds(store);
            let live = veles_core::user_envs::status_list().await;
            let mut out = String::new();
            out.push_str(&format!(
                "admin default zone: {}\n",
                if veles_core::user_envs::admin_default_is_sandbox() {
                    "sandbox (opt out with `host: true` per call, /sandbox host per session)"
                } else {
                    "host (fence one call with `host: false`, or name a box)"
                }
            ));
            if defs.is_empty() {
                out.push_str("declared: none (boxes are created implicitly on first use)\n");
            } else {
                out.push_str("declared:\n");
                for d in &defs {
                    out.push_str(&format!("  {}{}\n", d.name, def_detail(d)));
                }
            }
            if binds.is_empty() {
                out.push_str("assignments: none\n");
            } else {
                out.push_str("assignments (most specific wins):\n");
                for b in &binds {
                    out.push_str(&format!("  {}\n", bind_detail(b)));
                }
            }
            if live.is_empty() {
                out.push_str("live: no containers");
            } else {
                out.push_str("live:\n");
                for e in &live {
                    out.push_str(&format!("  {}\n", live_detail(e)));
                }
            }
            out.trim_end().to_string()
        }

        "show" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, "`/sandbox show`", Cap::CmdAdmin);
            }
            let Some(name) = args.first() else {
                return "usage: /sandbox show <name>".into();
            };
            let name = veles_core::sandboxes::normalize(name);
            let mut out = format!("sandbox {name}");
            match veles_core::sandboxes::defs(store)
                .into_iter()
                .find(|d| veles_core::sandboxes::normalize(&d.name) == name)
            {
                Some(d) => out.push_str(&format!("\ndeclared{}", def_detail(&d))),
                None => out.push_str("\nnot declared (implicit, or created on first use)"),
            }
            let binds: Vec<String> = veles_core::sandboxes::binds(store)
                .iter()
                .filter(|b| veles_core::sandboxes::normalize(&b.sandbox) == name)
                .map(bind_detail)
                .collect();
            if binds.is_empty() {
                out.push_str("\nnothing is assigned to it");
            } else {
                out.push_str("\nassigned to:");
                for b in binds {
                    out.push_str(&format!("\n  {b}"));
                }
            }
            let key = veles_core::user_envs::key_for(&name);
            out.push_str(&format!(
                "\n{}",
                live_detail(&veles_core::user_envs::status_one(&key).await)
            ));
            // Worth a line because a shared box is a shared resource:
            // the people routed here queue against each other, and a
            // caller who is told "already running N commands" should
            // be able to find out what N is.
            let limit = veles_core::user_envs::max_concurrent_exec();
            out.push_str(&if limit == 0 {
                "\nexec slots: unbounded (`[sandbox] max_concurrent_exec = 0`)".to_string()
            } else {
                format!("\nexec slots: {limit} at a time, shared by everyone routed here")
            });
            out
        }

        "create" | "define" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, "`/sandbox create`", Cap::CmdAdmin);
            }
            let Some(name) = args.first() else {
                return "usage: /sandbox create <name> [--image i] [--memory-mb n] \
                        [--network on|off] [--note \"why\"]"
                    .into();
            };
            let mut def = veles_core::config::SandboxDef {
                name: name.to_string(),
                image: None,
                memory_mb: None,
                network: None,
                note: None,
                created_by: None,
                created_at: None,
            };
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--image" => def.image = it.next().map(|s| s.to_string()),
                    "--memory-mb" => def.memory_mb = it.next().and_then(|s| s.parse().ok()),
                    "--network" => {
                        def.network = it.next().map(|s| matches!(*s, "on" | "true" | "yes" | "1"))
                    }
                    "--note" => {
                        // The rest of the line is the note.
                        let words: Vec<&str> = it.by_ref().copied().collect();
                        def.note = Some(words.join(" ").trim_matches('"').to_string());
                    }
                    _ => {}
                }
            }
            let by = principal
                .map(|p| p.subject.clone())
                .unwrap_or_else(|| "local:owner".to_string());
            match veles_core::sandboxes::define(ctx.store, def, &by) {
                Ok(d) => format!(
                    "sandbox {} declared{} — assign it with /sandbox assign",
                    d.name,
                    def_detail(&d)
                ),
                Err(e) => format!("create failed: {e:#}"),
            }
        }

        "drop" | "undefine" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, "`/sandbox drop`", Cap::CmdAdmin);
            }
            let Some(name) = args.first() else {
                return "usage: /sandbox drop <name>  (forgets the declaration; the container \
                        and its workspace survive — that is /sandbox purge)"
                    .into();
            };
            if veles_core::sandboxes::undefine(ctx.store, name) {
                format!("forgot the declaration for {name} (container and workspace untouched)")
            } else {
                format!("no runtime declaration named {name}")
            }
        }

        "assign" | "unassign" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, &format!("`/sandbox {sub}`"), Cap::CmdAdmin);
            }
            let Some(name) = args.first() else {
                return format!(
                    "usage: /sandbox {sub} <name> [--identity <id|mask>] [--network n] \
                     [--channel c] [--platform p] [--role r]"
                );
            };
            let mut b = veles_core::config::SandboxBind {
                identity: None,
                network: None,
                channel: None,
                platform: None,
                role: None,
                sandbox: name.to_string(),
            };
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                match *a {
                    "--identity" | "--mask" | "--id" => {
                        b.identity = it.next().map(|s| s.to_string())
                    }
                    "--network" | "--net" => b.network = it.next().map(|s| s.to_string()),
                    "--channel" | "--chan" => b.channel = it.next().map(|s| s.to_string()),
                    "--platform" => b.platform = it.next().map(|s| s.to_string()),
                    "--role" => b.role = it.next().map(|s| s.to_string()),
                    _ => {}
                }
            }
            if sub == "unassign" {
                if veles_core::sandboxes::unbind(ctx.store, &b) {
                    format!("unassigned: {}", bind_detail(&b))
                } else {
                    format!("no runtime assignment matching {}", bind_detail(&b))
                }
            } else {
                match veles_core::sandboxes::bind(ctx.store, b) {
                    Ok(b) => format!("assigned: {}", bind_detail(&b)),
                    Err(e) => format!("assign failed: {e:#}"),
                }
            }
        }

        // The session override. `host` is admin-only by construction:
        // below admin `zone_for` forces a sandbox whatever the session
        // says, so the honest thing is to refuse here rather than
        // accept and ignore.
        "use" | "host" => {
            // `cmd-status`, not `cmd-session`: the real gate is the
            // REACHABLE SET below, and it is a tighter one than any
            // tier — a caller may only name a box something already
            // assigned to them. Requiring cmd-session on top would
            // stop a plain user from choosing between two boxes that
            // are both theirs, which is not a privilege anyone needs
            // protecting from.
            if !caller_is(principal, Cap::CmdStatus) {
                return denied(principal, "`/sandbox use`", Cap::CmdStatus);
            }
            let want = if sub == "host" {
                "host"
            } else {
                args.first().copied().unwrap_or("")
            };
            if want.is_empty() {
                return "usage: /sandbox use <name>  |  /sandbox host  (admin)".into();
            }
            if want.eq_ignore_ascii_case("host") {
                if !caller_is(principal, Cap::CmdAdmin) {
                    return denied(principal, "leaving the sandbox", Cap::CmdAdmin);
                }
                return "the host is not a session setting — pass `host: true` on the \
                        call that needs it, or set [sandbox] admin_default_zone = \"host\""
                    .into();
            }
            let name = veles_core::sandboxes::normalize(want);
            if let Some(p) = principal {
                if !veles_core::sandboxes::may_use_session(p, &name) {
                    return veles_core::user_envs::deny_named_sandbox(&name, p);
                }
            }
            veles_core::sandboxes::set_session_sandbox(ctx.session_id, &name);
            format!("this session's tools now run in `{name}`")
        }

        "purge" => {
            if !caller_is(principal, Cap::CmdAdmin) {
                return denied(principal, "`/sandbox purge`", Cap::CmdAdmin);
            }
            let Some(name) = args.first() else {
                return "usage: /sandbox purge <name> [--work]   (--work also deletes the \
                        workspace, which holds people's files)"
                    .into();
            };
            let delete_work = args.contains(&"--work");
            let name = veles_core::sandboxes::normalize(name);
            let key = veles_core::user_envs::key_for(&name);
            match veles_core::user_envs::purge(&key, delete_work).await {
                Ok(()) => format!(
                    "purged {name}{} — it comes back on the next call",
                    if delete_work {
                        " and deleted its workspace"
                    } else {
                        " (workspace kept)"
                    }
                ),
                Err(e) => format!("purge failed: {e:#}"),
            }
        }

        other => format!(
            "unknown /sandbox subcommand {other:?} — where | list | show | create | drop | \
             assign | unassign | use | purge"
        ),
    }
}

fn def_detail(d: &veles_core::config::SandboxDef) -> String {
    let mut bits = Vec::new();
    if let Some(i) = &d.image {
        bits.push(format!("image {i}"));
    }
    if let Some(m) = d.memory_mb {
        bits.push(format!("{m}MB"));
    }
    if let Some(n) = d.network {
        bits.push(format!("network {}", if n { "on" } else { "off" }));
    }
    if let Some(n) = &d.note {
        bits.push(n.clone());
    }
    if bits.is_empty() {
        String::new()
    } else {
        format!(" ({})", bits.join(", "))
    }
}

fn bind_detail(b: &veles_core::config::SandboxBind) -> String {
    let mut sel = Vec::new();
    for (k, v) in [
        ("identity", &b.identity),
        ("platform", &b.platform),
        ("network", &b.network),
        ("channel", &b.channel),
        ("role", &b.role),
    ] {
        if let Some(v) = v {
            sel.push(format!("{k} {v}"));
        }
    }
    format!(
        "{} → {}",
        if sel.is_empty() {
            "everyone".to_string()
        } else {
            sel.join(" + ")
        },
        b.sandbox
    )
}

fn live_detail(e: &serde_json::Value) -> String {
    let s = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("");
    format!(
        "{} · {} · {}",
        if s("name").is_empty() {
            s("key").to_string()
        } else {
            format!("{} [{}]", s("name"), s("key"))
        },
        if e.get("running").and_then(|v| v.as_bool()) == Some(true) {
            "running"
        } else {
            "stopped"
        },
        s("workspace")
    )
}

/// `/asks`, `/answer <id> <choice>`, `/approve <id>`, `/deny <id>` —
/// the pending questions and the explicit way to decide one.
///
/// The colloquial path is unchanged: typing "1" or "yes" in a chat
/// resolves that chat's oldest ask through `ask::resolve_for_chat`.
/// This is the other half, for a channel with several in flight, and
/// it was an IRC-only `.agent` verb until the sigil fold made it a
/// command like any other.
///
/// `scope` is the chat whose asks are listed — `None` on an owner
/// surface, which sees every open ask. `decider` is who is answering,
/// checked against each ask's binding: `Cap::Approve` gets you as far
/// as the verb, and the BINDING decides whether this particular ask is
/// yours to answer.
/// Takes the PRINCIPAL rather than an `AclCtx`: nothing here reads the
/// store, and asking for one made the IRC path open `state.db` per
/// command just to satisfy a signature — which under parallel tests
/// answered "database is locked" and in production is a pointless
/// handle. A false dependency in a signature is still a dependency.
pub fn ask_command(
    principal: Option<&veles_core::acl::Principal>,
    scope: Option<&str>,
    decider: veles_core::ask::Decider,
    verb: &str,
    rest: &str,
) -> String {
    use veles_core::acl::Cap;
    let open = match scope {
        Some(chat) => veles_core::ask::open_for_chat(chat),
        None => veles_core::ask::open_all(),
    };
    if verb == "asks" {
        if open.is_empty() {
            return "no open questions".to_string();
        }
        return open
            .iter()
            .map(|r| {
                let opts: Vec<&str> = r.choices.iter().map(|c| c.key.as_str()).collect();
                format!("{}: {} [{}]", r.id, r.prompt, opts.join("/"))
            })
            .collect::<Vec<_>>()
            .join(" | ");
    }
    if !caller_is(principal, Cap::Approve) {
        return denied(principal, &format!("`{verb}`"), Cap::Approve);
    }
    let rest = rest.trim();
    let (id, choice) = match verb {
        "approve" => (rest.to_string(), veles_core::ask::CHOICE_ONCE.to_string()),
        "deny" => (rest.to_string(), veles_core::ask::CHOICE_DENY.to_string()),
        _ => match rest.split_once(char::is_whitespace) {
            Some((i, c)) => (i.trim().to_string(), c.trim().to_string()),
            None => return "usage: /answer <id> <choice>".to_string(),
        },
    };
    if id.is_empty() {
        return format!("usage: /{verb} <ask-id>");
    }
    // The binding, not just the capability. Without this a second
    // holder of `Cap::Approve` in a shared channel decides someone
    // else's approval by naming its id — the hole S15 closed for a
    // bare "1", reachable by the longer spelling.
    if !veles_core::ask::may_answer(&id, decider) {
        return format!(
            "no open question with id {id} that is yours to answer \
             (an ask belongs to whoever opened it, or to the owner)"
        );
    }
    if veles_core::ask::resolve(&id, veles_core::ask::AskAnswer::choice(&choice)) {
        format!("recorded: {id} → {choice}")
    } else {
        format!("no open question with id {id}")
    }
}

/// `/lockdown [on [reason] | off | status]` — the emergency brake.
///
/// Deliberately asymmetric, and the asymmetry is the whole feature:
/// **any admin may engage it from anywhere, and only a LOCAL surface
/// may lift it.** USER, 2026-09-09: *"Can be enabled from a connector
/// by an admin but disabled only from a local frontend."*
///
/// Engaging has to be reachable from the place the trouble is — a
/// channel, from a phone — or the brake is useless. Lifting must not
/// be, because an attacker who reached an admin mask would simply lift
/// it, and then the brake is worth exactly what that mask is worth.
/// Being at the desktop, the TUI or the REPL is different evidence in
/// kind: not a role in a config file but access to the machine.
///
/// Reading is open at the name (`cmd-status`) so a refused sender can
/// find out what refused them, and `lockdown.rs` refuses the connector
/// path anyway — the tier here is about the verbs.
pub fn lockdown_command(ctx: &AclCtx, surface: Surface, rest: &str) -> String {
    use veles_core::acl::Cap;
    let mut words = rest.split_whitespace();
    let sub = words.next().unwrap_or("").to_ascii_lowercase();
    let reason = rest
        .trim()
        .strip_prefix(sub.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    match sub.as_str() {
        "" | "status" | "show" => veles_core::lockdown::status(Some(ctx.store)),
        "on" | "engage" | "enable" => {
            if !caller_is(ctx.principal, Cap::CmdAdmin) {
                return denied(ctx.principal, "`lockdown on`", Cap::CmdAdmin);
            }
            let by = match ctx.principal {
                Some(p) => p.subject.clone(),
                None => format!("{}:owner", surface_name(surface)),
            };
            match veles_core::lockdown::engage(ctx.store, &by, &reason) {
                Ok(_) => {
                    // Refusing the NEXT message is not a brake while
                    // the current turn is still calling tools.
                    let stopped = veles_core::agent::cancel_connector_turns();
                    let also = match stopped {
                        0 => String::new(),
                        1 => " One running connector turn was cancelled.".to_string(),
                        n => format!(" {n} running connector turns were cancelled."),
                    };
                    format!(
                        "lockdown ENGAGED.{also} Connectors are refused from now on; the \
                         local frontends and cron keep working. Only a local frontend \
                         (desktop/TUI/REPL) can lift it.\n{}",
                        veles_core::lockdown::status(Some(ctx.store))
                    )
                }
                Err(e) => format!("lockdown failed to engage: {e:#}"),
            }
        }
        "off" | "release" | "disable" | "lift" => {
            if !surface.is_local() {
                return format!(
                    "[lockdown] refused: a lockdown is lifted only from a LOCAL frontend \
                     (desktop, TUI or REPL on the machine veles runs on) — never from a \
                     connector, whatever the caller's role. That asymmetry is the point: \
                     an admin credential that engaged it must not be enough to lift it.\n{}",
                    veles_core::lockdown::status(Some(ctx.store))
                );
            }
            match veles_core::lockdown::release(ctx.store) {
                Err(e) => format!("lockdown failed to lift: {e:#}"),
                Ok(had) => {
                    // A config-declared one survives the release, and
                    // saying "lifted" then would be a lie.
                    if veles_core::lockdown::engaged(Some(ctx.store)) {
                        veles_core::lockdown::status(Some(ctx.store))
                    } else if had {
                        "lockdown lifted — connectors are served again.".to_string()
                    } else {
                        "lockdown was not engaged; nothing to lift.".to_string()
                    }
                }
            }
        }
        other => format!(
            "usage: /lockdown [status | on [reason] | off]  (got `{other}`)\n{}",
            veles_core::lockdown::status(Some(ctx.store))
        ),
    }
}

fn surface_name(s: Surface) -> &'static str {
    match s {
        Surface::Repl => "repl",
        Surface::Tui => "tui",
        Surface::Desktop => "desktop",
        Surface::Gateway => "gateway",
    }
}

/// `/limits` — the per-role context ceilings, and the one place they
/// are changed.
///
/// Reading is open (`cmd-status`): a caller who is about to be refused
/// a turn is entitled to know the number. Setting is `cmd-admin`, and
/// it writes to the runtime store so every surface sees it live —
/// `config.toml` stays hand-authored.
pub fn limits_command(ctx: &AclCtx, rest: &str) -> String {
    use veles_core::acl::{Cap, Role};
    let cfg = acl_cfg(ctx);
    let mut words = rest.split_whitespace();
    let sub = words.next().unwrap_or("");

    let show = |cfg: &veles_core::acl::AclConfig| {
        // A number below a thousand prints exactly, at BOTH ends: a
        // 1-token ceiling read "0k" (see the ktok comment) and a spend
        // of 12 tokens read "0k" too, so the reading and the ceiling
        // were equally unreadable at small values.
        // Thousands are the readable unit for a token budget, but only
        // once there are thousands of them: `{n}/1000` renders a
        // 1-token ceiling as "0k", which is indistinguishable from
        // nothing and told the caller about to be refused precisely
        // nothing about why. Small numbers print exactly.
        let ktok = |n: u64| match n {
            0 => "unlimited".to_string(),
            n if n < 1000 => n.to_string(),
            n => format!("{}k", n / 1000),
        };
        let plain = |n: u64| {
            if n == 0 {
                "unlimited".to_string()
            } else {
                n.to_string()
            }
        };
        let roles = [
            Role::Guest,
            Role::User,
            Role::Trusted,
            Role::Admin,
            Role::Owner,
        ];
        // Three budgets, one table each. They answer different
        // questions and confusing them is how an operator tightens the
        // wrong one: context is how big ONE conversation may get,
        // tokens/day is the bill, turns/hour is the flood brake.
        let mut out = format!(
            "context ceiling (size of one conversation) · default {}",
            ktok(cfg.limits.default_max_context_tokens)
        );
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                ktok(cfg.limits.for_role(role))
            ));
        }
        out.push_str(&format!(
            "\ntokens per day (the bill) · default {}",
            ktok(cfg.limits.default_max_tokens_per_day)
        ));
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                ktok(cfg.limits.tokens_per_day_for_role(role))
            ));
        }
        out.push_str(&format!(
            "\nturns per hour (refused before any provider call) · default {}",
            plain(cfg.limits.default_max_turns_per_hour)
        ));
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                plain(cfg.limits.turns_per_hour_for_role(role))
            ));
        }
        out.push_str(&format!(
            "\nconcurrent turns per caller · {} (admins and owners are never held)",
            plain(u64::from(cfg.limits.max_concurrent_turns))
        ));
        // The command-region budgets. Separate tables because they are
        // separate money: a picture, a clip and an evaluation cost three
        // very different things, and `/gen` and `/tcl` are answered
        // above the turn brakes so none of them shows up in the rows
        // above.
        out.push_str(&format!(
            "\ngenerations per hour (`/gen image|audio`) · default {}",
            plain(cfg.limits.default_max_media_per_hour)
        ));
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                plain(cfg.limits.media_per_hour_for_role(role))
            ));
        }
        out.push_str(&format!(
            "\nclips per hour (`/gen video` — ~1200 credits each) · default {}",
            plain(cfg.limits.default_max_video_per_hour)
        ));
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                plain(cfg.limits.video_per_hour_for_role(role))
            ));
        }
        out.push_str(&format!(
            "\nevaluations per hour (`/tcl` — no provider call) · default {}",
            plain(cfg.limits.default_max_eval_per_hour)
        ));
        for role in roles {
            out.push_str(&format!(
                "\n  {:<8} {}",
                role.as_str(),
                plain(cfg.limits.eval_per_hour_for_role(role))
            ));
        }
        if let Some(p) = ctx.principal {
            // The caller's own row, with what they have actually spent
            // — a ceiling with no reading against it is not an answer
            // to "how much have I got left".
            let spent = ctx
                .store
                .lock()
                .ok()
                .and_then(|s| s.tokens_spent_since(&p.subject, 24 * 60 * 60).ok())
                .unwrap_or(0);
            let turns = ctx
                .store
                .lock()
                .ok()
                .map(|s| s.turns_claimed(&p.subject, 60 * 60))
                .unwrap_or(0);
            // `ktok` maps 0 to "unlimited", which is right for a
            // CEILING and nonsense for a READING: nobody has spent
            // "unlimited" tokens by spending none.
            let reading = |n: u64| {
                if n < 1000 {
                    n.to_string()
                } else {
                    format!("{}k", n / 1000)
                }
            };
            out.push_str(&format!(
                "\nyours ({}): context {} · spent {} of {} today · {} of {} turns this hour",
                p.subject,
                ktok(p.max_context_tokens(cfg)),
                reading(spent),
                ktok(
                    cfg.limits
                        .tokens_per_day_for_subject(p.role, &p.subject, &p.budget_key())
                ),
                turns,
                plain(
                    cfg.limits
                        .turns_per_hour_for_subject(p.role, &p.subject, &p.budget_key())
                ),
            ));
        }
        // Per-subject overrides, when any are set — the answer to "why
        // does THIS person have a different number from their role".
        if !cfg.limits.by_subject.is_empty() {
            out.push_str("\nsubject overrides (beat the role ceiling):");
            let mut subs: Vec<_> = cfg.limits.by_subject.iter().collect();
            subs.sort_by(|a, b| a.0.cmp(b.0));
            for (who, b) in subs {
                let mut parts = Vec::new();
                if let Some(n) = b.context {
                    parts.push(format!("context {}", ktok(n)));
                }
                if let Some(n) = b.tokens_per_day {
                    parts.push(format!("tokens/day {}", ktok(n)));
                }
                if let Some(n) = b.turns_per_hour {
                    parts.push(format!("turns/h {}", plain(n)));
                }
                if let Some(n) = b.media_per_hour {
                    parts.push(format!("media/h {}", plain(n)));
                }
                if let Some(n) = b.video_per_hour {
                    parts.push(format!("video/h {}", plain(n)));
                }
                if let Some(n) = b.eval_per_hour {
                    parts.push(format!("eval/h {}", plain(n)));
                }
                out.push_str(&format!("\n  {} — {}", who, parts.join(" · ")));
            }
        }
        out.push_str(
            "\nset: /limits set [context|tokens-per-day|turns-per-hour|concurrent-turns|\
             media-per-hour|video-per-hour|eval-per-hour] <role|default|subject> <n|0=unlimited> \
             (admin) — a subject is irc:acct or ident@host and beats the role",
        );
        out
    };

    match sub {
        "" | "list" | "show" => show(&cfg),
        "set" => {
            if !caller_is(ctx.principal, Cap::CmdAdmin) {
                return denied(ctx.principal, "`/limits set`", Cap::CmdAdmin);
            }
            // Three shapes accepted, because all three read naturally:
            //   /limits set <role> <n>                      (context)
            //   /limits set <budget> <role> <n>
            //   /limits set concurrent-turns <n>            (one number)
            let a = words.next().unwrap_or("");
            let b = words.next();
            let c = words.next();
            let (budget, which, raw) = match (veles_core::acl::Budget::parse(a), b, c) {
                // A leading budget name.
                (Some(budget), Some(x), Some(y)) => (budget, x, y),
                (Some(budget), Some(x), None) if !budget.is_per_role() => (budget, "default", x),
                // No budget name: the context ceiling, as before.
                (_, Some(x), None) => (veles_core::acl::Budget::Context, a, x),
                _ => {
                    return "usage: /limits set [context|tokens-per-day|turns-per-hour|\
                            concurrent-turns] <role|default> <n>  (0 = unlimited)"
                        .into()
                }
            };
            let Ok(value) = raw.parse::<u64>() else {
                return format!("{raw:?} is not a number");
            };
            let shown = if value == 0 {
                "unlimited".to_string()
            } else {
                match budget {
                    // Tokens read in thousands; turns and concurrency
                    // are small integers and reading "0k turns" for 900
                    // of them would be worse than useless.
                    veles_core::acl::Budget::Context | veles_core::acl::Budget::TokensPerDay => {
                        format!("{}k tokens", value / 1000)
                    }
                    _ => value.to_string(),
                }
            };
            match veles_core::acl::set_runtime_budget(ctx.store, budget, which, value) {
                Ok(()) => format!(
                    "{} {} for {which} is now {shown} — live on every surface",
                    budget.as_str(),
                    if budget.is_per_role() { "limit" } else { "cap" },
                ),
                Err(e) => format!("limits set failed: {e:#}"),
            }
        }
        "clear" | "unset" => {
            if !caller_is(ctx.principal, Cap::CmdAdmin) {
                return denied(ctx.principal, "`/limits clear`", Cap::CmdAdmin);
            }
            let a = words.next().unwrap_or("");
            let b = words.next();
            let (budget, which) = match (veles_core::acl::Budget::parse(a), b) {
                (Some(budget), Some(x)) => (budget, x),
                (Some(budget), None) if !budget.is_per_role() => (budget, "default"),
                _ if !a.is_empty() => (veles_core::acl::Budget::Context, a),
                _ => {
                    return "usage: /limits clear [context|tokens-per-day|turns-per-hour|\
                            concurrent-turns] <role|default>"
                        .into()
                }
            };
            if veles_core::acl::clear_runtime_budget(ctx.store, budget, which) {
                format!(
                    "{} for {which} is back to the configured value",
                    budget.as_str()
                )
            } else {
                format!("no runtime {} override for {which}", budget.as_str())
            }
        }
        other => format!("unknown /limits subcommand {other:?} (list|set|clear)"),
    }
}

/// Per-caller usage and quotas, every budget, with the rolling-window
/// readings against them. A caller asks about themselves; an admin names
/// a subject (`irc:acct` or `ident@host`). This is the per-person answer
/// `usage_report` (the whole-bot plan ledger) does not give.
///
/// The windows are the enforcement windows, not arbitrary: tokens/day
/// reads the rolling 24h the spend gate sums, and each rate budget reads
/// the rolling hour its ledger holds — so the numbers here are the exact
/// readings the gates will refuse on.
pub fn usage_quotas_command(ctx: &AclCtx, rest: &str) -> String {
    use veles_core::acl::Cap;
    use veles_core::store::Store;

    let cfg = acl_cfg(ctx);
    let arg = rest.trim();

    // Resolve WHO the report is about. No argument: the caller. An
    // argument names a subject, and that is an admin read — one person's
    // bill is not another guest's business.
    let (subject, budget_key, role) = if arg.is_empty() {
        let Some(p) = ctx.principal else {
            return "usage: /usage [subject] — with no argument, YOUR budgets and spend".into();
        };
        (p.subject.clone(), p.budget_key(), p.role)
    } else {
        if !caller_is(ctx.principal, Cap::CmdAdmin) {
            return denied(ctx.principal, "/usage <subject>", Cap::CmdAdmin);
        }
        // Resolve the argument the way it is SPELLED. A mask-shaped
        // argument (`nick!ident@host`, or a bare `ident@host`) is a wire
        // mask: resolve it with the mask in the sender context so the
        // mask ENTRIES (`*!~a@host`) match and the role + budget key come
        // out the way the live gateway computes them. An account-shaped
        // argument (`irc:acct`) resolves by identity. Passing a mask as
        // the `id` resolves nothing (no rule names it) and reads the
        // caller as a guest — the bug this branch existed to avoid.
        let probe = if veles_core::ircmask::looks_like_mask(arg) {
            let mask = if arg.contains('!') {
                arg.to_string()
            } else {
                // A bare `ident@host`: budget_key form. Give it a nick
                // segment so the mask parser and the mask entries see a
                // full mask; the budget_key drops the nick anyway.
                format!("*!{arg}")
            };
            // A network-scoped mask entry only matches when the context
            // carries that network, and the admin named no network. So
            // resolve the mask on EVERY network the config's IRC rules
            // reference and keep the highest role found: a is `user` on
            // synirc (its entry's network), a stranger everywhere else.
            // Without this the report read a network-regular as a guest.
            let mut nets: Vec<String> = cfg
                .entries
                .iter()
                .filter_map(|e| e.network.clone())
                .collect();
            nets.sort();
            nets.dedup();
            let resolve_on = |net: Option<&str>| {
                let sctx = veles_core::acl::SenderContext {
                    mask: Some(mask.clone()),
                    network: net.map(str::to_string),
                    platform: Some("irc".into()),
                    ..Default::default()
                };
                veles_core::acl::Principal::resolve_ctx(None, &sctx, &cfg)
            };
            let mut best = resolve_on(None);
            for net in &nets {
                let p = resolve_on(Some(net));
                if p.role > best.role {
                    best = p;
                }
            }
            best
        } else {
            veles_core::acl::Principal::resolve_ctx(
                Some(arg),
                &veles_core::acl::SenderContext::default(),
                &cfg,
            )
        };
        (arg.to_string(), probe.budget_key(), probe.role)
    };

    // CEILING formatter: 0 means "no cap", which reads "unlimited".
    let ktok = |n: u64| match n {
        0 => "unlimited".to_string(),
        n if n < 1000 => n.to_string(),
        n => format!("{}k", n / 1000),
    };
    let plain = |n: u64| {
        if n == 0 {
            "unlimited".to_string()
        } else {
            n.to_string()
        }
    };
    // READING formatter: a spend of 0 is "0", never "unlimited" — the
    // same distinction `/limits` already draws between a ceiling and a
    // reading, and the bug "unlimited billed" was.
    let kread = |n: u64| {
        if n < 1000 {
            n.to_string()
        } else {
            format!("{}k", n / 1000)
        }
    };

    let store = match ctx.store.lock() {
        Ok(s) => s,
        Err(_) => return "usage: the store is busy".into(),
    };

    let l = &cfg.limits;
    let day = 24 * 60 * 60;
    let hour = 60 * 60;

    // The 24h spend, both as the discounted bill the gate sums and as
    // the raw split that shows the warm-prefix discount doing its work.
    let parts = store
        .spend_parts_since(&budget_key, day)
        .unwrap_or_default();
    let billed = parts.billed();
    let spend_ceiling = l.tokens_per_day_for_subject(role, &subject, &budget_key);

    // Each rolling-hour rate budget: claimed of the ceiling. context is
    // a size, not a rate, so it has no "spent" reading — it is listed
    // for completeness as the ceiling only.
    let turns_used = store.rate_claimed(Store::TURN_RATE_PREFIX, &budget_key, hour);
    let media_used = store.rate_claimed(Store::MEDIA_RATE_PREFIX, &budget_key, hour);
    let video_used = store.rate_claimed(Store::VIDEO_RATE_PREFIX, &budget_key, hour);
    let eval_used = store.rate_claimed(Store::EVAL_RATE_PREFIX, &budget_key, hour);

    let turns_cap = l.turns_per_hour_for_subject(role, &subject, &budget_key);
    let media_cap = l.media_per_hour_for_subject(role, &subject, &budget_key);
    let video_cap = l.video_per_hour_for_subject(role, &subject, &budget_key);
    let eval_cap = l.eval_per_hour_for_subject(role, &subject, &budget_key);
    let context_cap = l.for_subject(role, &subject, &budget_key);

    // Did a subject override set any of these, or are they the role's?
    // Naming the source is how the reader knows which knob to turn.
    // Matched case-insensitively, same as the resolution itself.
    let overridden = l
        .by_subject
        .keys()
        .any(|k| k.eq_ignore_ascii_case(&subject) || k.eq_ignore_ascii_case(&budget_key));
    let source = if overridden {
        "per-subject override"
    } else {
        match role {
            veles_core::acl::Role::Admin | veles_core::acl::Role::Owner => "owner/admin (uncapped)",
            _ => "role default",
        }
    };

    let mut out = format!(
        "usage for {subject} ({}, {}):\n  tokens/day: {} billed of {} \
         ({} calls: {} uncached + {} cached/6 + {} completion; rolling 24h)\n  \
         turns/hour: {} of {} · media/hour: {} of {} · video/hour: {} of {} · \
         eval/hour: {} of {} (rolling hour)\n  context ceiling: {} (one conversation)",
        role.as_str(),
        source,
        kread(billed),
        ktok(spend_ceiling),
        parts.calls,
        kread(parts.prompt_tokens.saturating_sub(parts.cached_tokens)),
        kread(parts.cached_tokens),
        kread(parts.completion_tokens),
        turns_used,
        plain(turns_cap),
        media_used,
        plain(media_cap),
        video_used,
        plain(video_cap),
        eval_used,
        plain(eval_cap),
        ktok(context_cap),
    );
    out.push_str("\n  set with /limits set <budget> <subject> <n> (admin)");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-turn-lock dispatcher must not substitute an owner Agent for
    /// the actual IRC caller. A recording backend exposes the interpreter
    /// selection and the exact wire identity that crossed that boundary.
    type RecordedTclCall = (bool, String, Option<veles_core::acl::SenderContext>);
    type RecordedTclCalls = std::sync::Arc<tokio::sync::Mutex<Vec<RecordedTclCall>>>;

    struct RecordingTcl {
        calls: RecordedTclCalls,
    }

    impl veles_core::tcleval::TclBackend for RecordingTcl {
        fn configured(&self) -> bool {
            true
        }

        fn eval<'a>(
            &'a self,
            code: &'a str,
            full: bool,
            caller: &'a str,
            sender: Option<&'a veles_core::acl::SenderContext>,
        ) -> veles_core::tcleval::EvalFuture<'a> {
            let calls = self.calls.clone();
            let code = code.to_string();
            let caller = caller.to_string();
            let sender = sender.cloned();
            Box::pin(async move {
                calls.lock().await.push((full, caller, sender));
                (format!("evaluated: {code}"), None).into()
            })
        }
    }

    #[tokio::test]
    async fn no_session_dispatch_preserves_the_tcl_caller_and_interpreter_split() {
        use veles_core::acl::{Principal, Role, SenderContext};
        static BACKEND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _lock = BACKEND_LOCK.lock().await;
        let (home, _env) = veles_core::testenv::scratch_home();
        std::fs::write(
            home.path().join("config.toml"),
            "[acl]\nowner = [\"irc:op\"]\ndefault_role = \"user\"\n",
        )
        .expect("configured ACL");
        veles_core::acl::invalidate_live_config();
        let calls = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        veles_core::tcleval::set_backend(std::sync::Arc::new(RecordingTcl {
            calls: calls.clone(),
        }));
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            veles_core::store::Store::open(
                &veles_core::config::veles_home().join("no-session-dispatch.db"),
            )
            .expect("scratch store"),
        ));
        let principal = Principal::known("irc:op", Role::Owner);
        let sender = SenderContext {
            mask: Some("op!ident@wire.example".into()),
            network: Some("fixture".into()),
            channel: Some("#chan".into()),
            platform: Some("irc".into()),
            chat_id: Some("#chan".into()),
            direct: false,
            handle: None,
            post: None,
        };
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: Some(&principal),
            sender: Some(&sender),
        };
        let ordinary = dispatch_async_as(&ctx, "/tcl", "expr {6*7}")
            .await
            .expect("ordinary Tcl is shared")
            .text;
        let admin = dispatch_async_as(&ctx, "/tcladmin", "info patchlevel")
            .await
            .expect("admin Tcl is shared")
            .text;
        let unknown = dispatch_async_as(&ctx, "/model", "anything").await;
        veles_core::tcleval::clear_backend();

        assert_eq!(ordinary, "evaluated: expr {6*7}");
        assert_eq!(admin, "evaluated: info patchlevel");
        assert!(unknown.is_none(), "a session command needs an Agent");
        let seen = calls.lock().await.clone();
        assert_eq!(seen.len(), 2);
        assert!(!seen[0].0, "plain /tcl never silently requests admin");
        assert!(seen[1].0, "the configured owner may request /tcladmin");
        for (_, caller, actual_sender) in seen {
            assert_eq!(caller, "op", "the wire nick, not an owner substitute");
            assert_eq!(actual_sender, Some(sender.clone()));
        }
    }

    /// Every shared command must be documented where a user of that
    /// surface will look. The REPL's `help()` listed about 30 of its 48
    /// commands, so several of these were reachable and undiscoverable —
    /// which for a user is not very different from missing.
    #[test]
    fn every_surface_help_names_every_shared_command() {
        // RM4.4: the three hand-maintained listings became one
        // generated table. The check is behavioral now — call
        // help_text, not parse chat.rs — and covers all three
        // surfaces, because the old test caught the REPL lying while
        // the TUI's and the gateway's texts went unchecked.
        //
        // The alias exemptions used to be a hand-written `matches!`
        // list, which is the same failure this file exists to end, one
        // layer down: it had already missed `/tcladmin` joining the
        // `/tcl` arm. They are DERIVED now — one arm of `dispatch` is
        // one command however many spellings it answers to, so
        // documenting it under any of them documents it, and an alias
        // that stops being an alias stops being exempt on its own.
        let arms = dispatch_arms();
        for (name, surface) in [
            ("REPL", Surface::Repl),
            ("TUI", Surface::Tui),
            ("gateway", Surface::Gateway),
            ("desktop", Surface::Desktop),
        ] {
            let help = help_text(surface);
            // Row NAMES, not substrings: `/tcl` would otherwise be
            // "documented" by a `/tcladmin` row, and `/todo` by
            // `/todos`.
            let rows: std::collections::BTreeSet<&str> = help
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter(|t| t.starts_with('/'))
                .collect();
            for c in SHARED_COMMANDS {
                let spellings = arms
                    .iter()
                    .find(|g| g.iter().any(|n| n == c))
                    .cloned()
                    .unwrap_or_else(|| vec![c.to_string()]);
                assert!(
                    spellings.iter().any(|n| rows.contains(n.as_str())),
                    "`{c}` works on the {name} but its /help names none of {spellings:?}"
                );
            }
        }
    }

    /// Discovery is tiered the way execution is: a guest asking for
    /// help is shown what they can run, told how many rows they
    /// cannot, and NOT handed the list of them.
    ///
    /// Before this, execution was gated and discovery was wide open —
    /// `/grant`, `/lockdown` and `/sandbox purge` sat in a guest's
    /// help output, and the only way to learn which half was real was
    /// to be refused one command at a time.
    #[test]
    fn help_is_filtered_to_what_the_caller_can_run() {
        let full = help_text_for(Surface::Gateway, None);
        assert!(
            full.contains("/grant") && full.contains("/lockdown"),
            "no principal is a local owner surface: nothing is withheld"
        );
        assert!(
            !full.contains("need a capability you do not have"),
            "and nothing is claimed to be withheld either:\n{full}"
        );

        let guest = veles_core::acl::Principal::guest();
        let seen = help_text_for(Surface::Gateway, Some(&guest));
        // Row NAMES, not substrings: `/acl`'s own description reads
        // "explain/grant/revoke (admin)", which is honest labelling
        // and not a row.
        let names: Vec<&str> = seen
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter(|t| t.starts_with('/'))
            .collect();
        // `/pair` is deliberately NOT in this list: redeeming a code
        // is open because the code IS the credential (creating one is
        // owner-only, inside the command).
        for admin_only in ["/grant", "/revoke", "/secret", "/crons"] {
            assert!(
                !names.contains(&admin_only),
                "a guest was shown the {admin_only} row:\n{seen}"
            );
        }
        for open in ["/status", "/context", "/caps", "/acl"] {
            assert!(
                names.contains(&open),
                "the rows a guest CAN run must stay ({open}):\n{seen}"
            );
        }
        assert!(
            seen.contains("more need a capability you do not have"),
            "the withheld rows are counted, not silently dropped:\n{seen}"
        );

        // The count is the real number, not a placeholder.
        let withheld = help_entries(Surface::Gateway)
            .into_iter()
            .filter(|(usage, _)| {
                let name = usage
                    .split_whitespace()
                    .next()
                    .unwrap_or(usage)
                    .trim_start_matches('/');
                veles_core::acl::builtin_cap(name).is_some_and(|c| !guest.has(c))
            })
            .count();
        assert!(withheld > 0, "the gateway table must have admin rows");
        assert!(
            seen.contains(&format!("({withheld} more need")),
            "the count must be the real one ({withheld}):\n{seen}"
        );
    }

    /// A table row per command, and no command in two rows — a stale
    /// duplicate row is how the old hand texts drifted.
    #[test]
    fn help_table_rows_are_unique() {
        for surface in [
            Surface::Repl,
            Surface::Tui,
            Surface::Gateway,
            Surface::Desktop,
        ] {
            let entries = help_entries(surface);
            let mut names: Vec<&str> = entries.iter().map(|(c, _)| *c).collect();
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                total,
                "{surface:?} help table has a duplicate row"
            );
        }
    }

    /// The unified context bill (R4.7): every number carries its unit —
    /// chars AND tokens — and the report names the capped bar, the warn
    /// line, and where history sits against both. One renderer behind
    /// every text surface; a surface printing a bare number would be a
    /// re-introduction of the report that confused units in the first
    /// place.
    /// The renderer half of veles-core's
    /// `the_context_report_carries_every_key_the_frontends_print`: a
    /// guest's LIVE report (not a hand-built one) renders its ceiling and
    /// the way out of it.
    #[tokio::test]
    async fn a_guests_live_context_report_names_its_ceiling() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let url = veles_core::providers::tests::sse_server_seq(vec![]).await;
        let agent = veles_core::testenv::mock_agent(&url, "context-guest");
        agent.set_principal(veles_core::acl::Principal::known(
            "irc:stranger",
            veles_core::acl::Role::Guest,
        ));
        let text = format_context_report(&agent.context_report(None));
        assert!(text.contains("your ceiling: 150000 tokens"), "{text}");
        assert!(text.contains("/compress"), "{text}");
    }

    #[test]
    fn the_context_bill_labels_every_unit_and_grounds_history() {
        let r = serde_json::json!({
            "model": "qwen3.8-flash",
            "context_window_tokens": 1_000_000,
            "compaction_trigger_percent": 85.0,
            "compaction_max_context_tokens": 450_000,
            "compaction_warn_tokens": 200_000,
            "compaction_budget_chars": 1_800_000,
            "history_chars": 900_000,
            "prefix_blocks": [
                {"name": "stable tier", "chars": 3_100, "tokens_est": 775},
                {"name": "tool schemas", "chars": 8_400, "tokens_est": 2_100}
            ],
            "session_calls": 7,
            "session_messages": 26,
            "session_prompt_tokens": 30_000,
            "session_completion_tokens": 4_000,
            "prefix_tokens_est": 0
        });
        let out = format_context_report(&r);

        // The cap is part of the story — the claimed 1M window must not
        // be what decides the bar.
        assert!(out.contains("min(85% of window, 450000 tokens)"), "{out}");
        assert!(out.contains("1800000 chars (≈450000 tokens)"), "{out}");
        assert!(out.contains("warn line: 200000 tokens"), "{out}");
        // History: 900k chars ≈ 225k tokens is OVER the 200k warn line
        // and 50% of the bar.
        assert!(out.contains("900000 chars (≈225000 tokens)"), "{out}");
        assert!(out.contains("50.0% of the bar"), "{out}");
        assert!(out.contains("OVER the warn line"), "{out}");
        // Prefix blocks carry both units (alignment may pad them).
        assert!(out.contains("3100 chars"), "{out}");
        assert!(out.contains("775 tok"), "{out}");
        assert!(out.contains("8400 chars"), "{out}");
        assert!(out.contains("2100 tok"), "{out}");
    }

    /// Under the warn line the relation is stated, not implied.
    #[test]
    fn the_context_bill_says_under_when_history_is_under() {
        let r = serde_json::json!({
            "model": "m",
            "context_window_tokens": 128_000,
            "compaction_trigger_percent": 85.0,
            "compaction_max_context_tokens": 450_000,
            "compaction_warn_tokens": 200_000,
            "compaction_budget_chars": 435_200,
            "history_chars": 40_000,
            "prefix_blocks": [],
            "session_calls": 1,
            "session_messages": 2,
            "session_prompt_tokens": 1_000,
            "session_completion_tokens": 100,
            "prefix_tokens_est": 0
        });
        let out = format_context_report(&r);
        assert!(out.contains("under the warn line"), "{out}");
        assert!(!out.contains("OVER"), "{out}");
    }

    /// The taxonomy, in the `hooks.rs` shape: a name is shared or it is
    /// surface-specific, never both and never neither.
    #[test]
    fn the_two_lists_are_disjoint() {
        for c in SHARED_COMMANDS {
            assert!(
                !SURFACE_SPECIFIC.contains(c),
                "`{c}` is in both lists — decide which it is"
            );
            assert!(
                !DUPLICATED_ACROSS_SURFACES.contains(c),
                "`{c}` is shared AND listed as duplicated — it was moved into \
                 the dispatcher, so delete it from DUPLICATED_ACROSS_SURFACES"
            );
        }
        for c in SURFACE_SPECIFIC {
            assert!(
                !DUPLICATED_ACROSS_SURFACES.contains(c),
                "`{c}` cannot be both surface-specific and duplicated across surfaces"
            );
        }
    }

    /// Every slash command implemented in MORE THAN ONE surface must be
    /// accounted for: shared (one implementation), surface-specific (only
    /// one surface has it), or declared debt.
    ///
    /// This closes the hole the other tests left. They enforce that
    /// DECLARED shared commands reach all three surfaces — and say
    /// nothing whatever about a command that was never declared. On
    /// 2026-09-04 thirty-one were in exactly that position: implemented
    /// twice, policed by nothing, free to diverge. That is the failure
    /// `commands.rs` was built to end, continuing quietly beside the
    /// tests meant to end it.
    ///
    /// The literal-scan is deliberately crude — a quoted `"/name"` in
    /// either source counts. A false positive costs one line in
    /// DUPLICATED_ACROSS_SURFACES; a false negative would let the thing
    /// this test exists for happen again.
    #[test]
    fn no_undeclared_command_is_implemented_twice() {
        // The same scan `dispatch_arms` runs, over a whole source
        // rather than one match: one scanner, so the inventory and the
        // duplicate-hunt can never disagree about what a command name
        // looks like.
        fn commands_in(src: &str) -> std::collections::BTreeSet<String> {
            slash_literals(src).into_iter().collect()
        }
        let chat = commands_in(include_str!("chat.rs"));
        let tui = commands_in(include_str!("tui.rs"));
        // The adapters were never scanned, which is how IRC grew a
        // 16-arm private `.agent` match and telegram three pre-bus
        // intercepts while this file policed only the in-process
        // surfaces. RM4 finding #3: ~3/4 of the command surface was
        // unreachable from IRC, and what WAS reachable was reachable
        // twice.
        let irc = commands_in(include_str!("../../veles-irc/src/client.rs"));
        let telegram = commands_in(include_str!("../../veles-telegram/src/lib.rs"));
        let declared: std::collections::BTreeSet<String> = SHARED_COMMANDS
            .iter()
            .chain(SURFACE_SPECIFIC)
            .chain(DUPLICATED_ACROSS_SURFACES)
            .chain(ADAPTER_LOCAL)
            .map(|s| s.to_string())
            .collect();

        for (a_name, a) in [
            ("chat.rs", &chat),
            ("tui.rs", &tui),
            ("irc", &irc),
            ("telegram", &telegram),
        ] {
            for (b_name, b) in [
                ("chat.rs", &chat),
                ("tui.rs", &tui),
                ("irc", &irc),
                ("telegram", &telegram),
            ] {
                if a_name >= b_name {
                    continue; // each pair once
                }
                let undeclared: Vec<&String> = a
                    .intersection(b)
                    .filter(|c| !declared.contains(*c))
                    .collect();
                assert!(
                    undeclared.is_empty(),
                    "these commands are implemented in BOTH {a_name} and {b_name} \
                     and declared nowhere: {undeclared:?}\n\
                     Two implementations of one command WILL diverge — that is the \
                     whole reason commands.rs exists. Move it into `dispatch` and add \
                     it to SHARED_COMMANDS, or declare it (DUPLICATED_ACROSS_SURFACES \
                     for debt, ADAPTER_LOCAL for an adapter's own surface) and accept \
                     it explicitly."
                );
            }
        }

        // …and the debt list may only shrink. An entry that is no longer
        // duplicated has been fixed, and leaving it here would make the
        // list lie about how much debt is left.
        let stale: Vec<&&str> = DUPLICATED_ACROSS_SURFACES
            .iter()
            .filter(|c| !(chat.contains(**c) && tui.contains(**c)))
            .collect();
        assert!(
            stale.is_empty(),
            "no longer duplicated — delete from DUPLICATED_ACROSS_SURFACES: {stale:?}"
        );
    }

    /// The enforced half of the module doc. Every shared command must be
    /// reachable from all three text surfaces, and the check is textual
    /// against the real sources for the same reason the hook tests are:
    /// a dispatcher that compiles proves nothing about whether anyone
    /// calls it.
    #[test]
    fn every_shared_command_reaches_every_surface() {
        let chat = include_str!("chat.rs");
        let tui = include_str!("tui.rs");
        let gateway = include_str!("gateway.rs");
        for (name, src) in [("chat.rs", chat), ("tui.rs", tui), ("gateway.rs", gateway)] {
            assert!(
                src.contains("commands::dispatch"),
                "{name} never calls the shared dispatcher, so every command in \
                 SHARED_COMMANDS is unreachable from it"
            );
        }
    }

    /// A shared command must not ALSO have a private arm in an
    /// in-process surface: two implementations of `/mech` is how they
    /// drifted apart the first time. The gateway is exempt only for the
    /// names it declares in `GATEWAY_OVERRIDES`.
    #[test]
    fn no_surface_keeps_a_private_copy_of_a_shared_command() {
        let chat = include_str!("chat.rs");
        let tui = include_str!("tui.rs");
        // Only command-dispatch bodies can contain a private command
        // implementation. The rest of this large adapter file also has
        // help inventories and the shared-capability table, which
        // legitimately spell `"tcl" | "eval"`; a whole-file substring
        // scan rejected those declarations as though they evaluated Tcl.
        let irc_file = include_str!("../../veles-irc/src/client.rs");
        let irc = irc_file
            .split("async fn handle_irc_command(")
            .nth(1)
            .expect("IRC command dispatch")
            .split("\n}\n")
            .next()
            .expect("end of IRC command dispatch");
        let telegram = include_str!("../../veles-telegram/src/lib.rs");
        for c in SHARED_COMMANDS {
            let arm = format!("\"{c}\" =>");
            let alt = format!("\"{c}\" |");
            let tui_overridden = TUI_OVERRIDES.contains(c);
            for (name, src) in [
                ("chat.rs", chat),
                ("tui.rs", tui),
                ("irc/client.rs", irc),
                ("telegram/lib.rs", telegram),
            ] {
                if name == "tui.rs" && tui_overridden {
                    continue; // declared TUI_OVERRIDES
                }
                assert!(
                    !src.contains(&arm) && !src.contains(&alt),
                    "{name} still has its own `{c}` arm — delete it and let \
                     commands::dispatch serve it"
                );
            }
        }
    }

    /// An override is only honest while it is still an override of
    /// something real, and while the surface actually implements it.
    #[test]
    fn a_tui_override_is_declared_and_real() {
        let tui = include_str!("tui.rs");
        for c in TUI_OVERRIDES {
            assert!(
                SHARED_COMMANDS.contains(c),
                "`{c}` overrides a shared command that does not exist — stale list"
            );
            assert!(
                tui.contains(&format!("\"{c}\"")),
                "the TUI declares `{c}` an override but never mentions it — stale list"
            );
        }
    }

    /// A Guest must not be able to cancel the chat's turn.
    ///
    /// The gateway answers `/cancel` BEFORE the per-chat lock (it has
    /// to — that ordering is the feature), and until 2026-09-07 that
    /// fast path checked nothing past Banned. `acl.rs` says `cancel`
    /// is CmdSession; the fast path just didn't ask. This pins both
    /// halves: the tier, and the check that enforces it.
    #[test]
    fn a_guest_cannot_cancel_someone_elses_turn() {
        assert_eq!(
            veles_core::acl::builtin_cap("cancel"),
            Some(veles_core::acl::Cap::CmdSession)
        );
        assert_eq!(
            veles_core::acl::builtin_cap("stop"),
            Some(veles_core::acl::Cap::CmdSession)
        );
        let guest = veles_core::acl::Principal::guest();
        assert!(
            !guest.has(veles_core::acl::Cap::CmdSession),
            "a guest must lack CmdSession, or the tier is theatre"
        );
        // And the fast path must actually ask: the check is textually
        // present between the `/cancel` match and the cancel itself,
        // because ordering is exactly what made it skippable.
        let gateway = include_str!("gateway.rs");
        let fast_path = gateway
            .find("if t == \"/cancel\" || t == \"/stop\" {")
            .expect("the /cancel fast path");
        let cancel_call = gateway[fast_path..]
            .find("cancel_turn")
            .expect("the cancel itself")
            + fast_path;
        let gate = gateway[fast_path..cancel_call]
            .find("principal.has(veles_core::acl::Cap::CmdSession)")
            .expect("the ACL check before the cancel");
        assert!(
            gate < (cancel_call - fast_path),
            "the ACL gate must precede the cancel call"
        );
    }

    /// `compact`/`compress` rewrites the session's history; until
    /// 2026-09-07 it was reachable by anyone who could chat, because
    /// `builtin_cap` said None and the gateway read None as "no gate".
    #[test]
    fn compaction_has_a_tier() {
        assert_eq!(
            veles_core::acl::builtin_cap("compact"),
            Some(veles_core::acl::Cap::CmdSession)
        );
        assert_eq!(
            veles_core::acl::builtin_cap("compress"),
            Some(veles_core::acl::Cap::CmdSession)
        );
    }

    /// Session lifecycle commands (DUPLICATED_ACROSS_SURFACES, now
    /// gateway-reachable) are session-shaping: a guest in a shared
    /// channel must not be able to /new away the owner's session.
    #[test]
    fn session_lifecycle_commands_have_a_tier() {
        for c in ["new", "reset", "resume", "retry"] {
            assert_eq!(
                veles_core::acl::builtin_cap(c),
                Some(veles_core::acl::Cap::CmdSession),
                "{c} must be CmdSession on the gateway"
            );
        }
    }

    /// An override is a shared name the gateway answers differently on
    /// purpose. It must BE a shared name (or the list is stale), and the
    /// gateway must actually implement it (or the override is a hole:
    /// the shared version is shadowed by nothing at all).
    #[test]
    fn a_gateway_override_is_declared_and_real() {
        let gateway = include_str!("gateway.rs");
        for c in GATEWAY_OVERRIDES {
            assert!(
                SHARED_COMMANDS.contains(c),
                "`{c}` overrides a shared command that does not exist — stale list"
            );
            // Two legal shapes. A match arm in the builtin table —
            // allowing alternation, since `"acl" | "access" =>` is one
            // arm serving two names — or an explicit comparison, which
            // is what `/cancel` needs: it has to be answered BEFORE the
            // per-chat lock, and the builtin table sits after it.
            let bare = format!("\"{}\"", c.trim_start_matches('/'));
            let slashed = format!("\"{c}\"");
            let has_impl = gateway.lines().any(|l| {
                (l.contains(&bare) || l.contains(&slashed))
                    && (l.contains("=>") || l.contains("=="))
            });
            assert!(
                has_impl,
                "gateway declares `{c}` an override but has no implementation for \
                 it, so the shared version answers after all"
            );
        }
    }

    /// Every shared command must have an ACL tier.
    ///
    /// Found by wiring, and it is the sharp edge of this whole change:
    /// `acl::builtin_cap` returns `None` for "not a builtin", and the
    /// gateway reads `None` as "no gate, carry on". So the moment a
    /// command becomes reachable from the gateway WITHOUT an entry in
    /// that table, any sender who may chat can run it. `/mech` — which
    /// sets what the engine may do unattended — was in exactly that
    /// state for as long as it took this test to run.
    #[test]
    fn every_shared_command_has_an_acl_tier() {
        for c in SHARED_COMMANDS {
            let bare = c.trim_start_matches('/');
            assert!(
                veles_core::acl::builtin_cap(bare).is_some(),
                "`{c}` is reachable from the gateway but acl::builtin_cap says it \
                 is not a builtin, which the gateway reads as 'no gate' — give it \
                 a tier in acl.rs"
            );
        }
    }

    /// The contents of every `"lowercase"` / `"/lowercase"` string
    /// literal in a chunk of source, in order of appearance.
    fn quoted_names(src: &str) -> Vec<String> {
        let chars: Vec<char> = src.chars().collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] != '"' {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            if j < chars.len() && chars[j] == '/' {
                j += 1;
            }
            let start = j;
            while j < chars.len() && chars[j].is_ascii_lowercase() {
                j += 1;
            }
            if j < chars.len() && chars[j] == '"' && j > start {
                out.push(chars[i + 1..j].iter().collect());
            }
            i = j + 1;
        }
        out
    }

    /// Just the slash-prefixed ones: command names as they are written.
    fn slash_literals(src: &str) -> Vec<String> {
        quoted_names(src)
            .into_iter()
            .filter(|n| n.starts_with('/'))
            .collect()
    }

    /// Every arm that BLOCKS on a fresh runtime has an awaited twin.
    ///
    /// `dispatch` is sync, so a genuinely async command makes its own
    /// runtime and `block_on`s it. From a sync caller that is correct;
    /// from an async one it panics with "Cannot start a runtime from
    /// within a runtime", and `lifecycle` turns a panic into an abort.
    /// The failure is therefore not an error message but the whole
    /// gateway dying — a desktop user typing `/tcl` dumped core
    /// (2026-09-12).
    ///
    /// `command_run` had carved out `/sandbox` BY HAND, which fixed the
    /// one command somebody had hit and silently left `/gen`, `/tcl`,
    /// `/eval`, `/tcladmin`, `/compress` and `/compact` fatal. That is
    /// the same shape as a taxonomy with audits and no derivation, so
    /// the set is read out of the dispatcher's source here instead of
    /// being written down twice.
    #[test]
    fn every_blocking_arm_has_an_awaited_twin() {
        let src = include_str!("commands.rs");
        let body = src
            .split("pub fn dispatch(")
            .nth(1)
            .expect("the shared dispatcher");
        let end = body.find("\n}\n").expect("the end of the dispatcher");
        let body = &body[..end];

        // Walk the arms in order, remembering the most recent head; a
        // line that makes a runtime belongs to whichever head is open.
        //
        // A head with no `"/name"` literal CLOSES the previous one rather
        // than leaving it open. Without that, the guarded alias arm at
        // the bottom of the dispatcher — `_ if alias_gen_kind(…)`, which
        // does block — was attributed to `/compress`, the last arm that
        // happened to carry a literal. The test still passed, for the
        // wrong reason, and any future literal-less blocking arm would
        // have inherited whatever name preceded it. A name-less arm is
        // covered by `both_dispatchers_serve_media_aliases` instead,
        // which compares the two fallthroughs by HANDLER rather than by
        // literal.
        let mut current: Vec<String> = Vec::new();
        let mut blocking: Vec<String> = Vec::new();
        for line in body.lines() {
            if let Some((head, _)) = line.split_once("=>") {
                current = slash_literals(head);
            }
            if line.contains("tokio::runtime::Runtime::new()") {
                blocking.extend(current.iter().cloned());
            }
        }
        assert!(
            !blocking.is_empty(),
            "the scraper found no blocking arms at all — it has drifted from the \
             source it reads, which would make this test pass by seeing nothing"
        );

        let twin = src
            .split("pub async fn dispatch_async(")
            .nth(1)
            .expect("the awaited twin");
        let twin_end = twin.find("\n}\n").expect("the end of the twin");
        let twin = &twin[..twin_end];
        for name in &blocking {
            assert!(
                twin.contains(&format!("\"{name}\"")),
                "`{name}` blocks on a fresh runtime in `dispatch` but `dispatch_async` \
                 does not answer it. An async caller reaching that arm does not get an \
                 error — it aborts the process. Add it to the twin."
            );
        }
    }
    /// Both dispatchers reach the alias handler, and neither one does it
    /// with a private copy.
    ///
    /// This is the replacement for the coverage
    /// `every_blocking_arm_has_an_awaited_twin` cannot give. That test
    /// reads quoted slash-names out of match arm heads, and a
    /// config-declared alias has no literal anywhere in the source — the
    /// name lives in the user's `config.toml`. So the one arm in this
    /// module whose blocking half could drift from its awaited half is
    /// precisely the one the scraper is blind to.
    ///
    /// What is checked instead is the shape: the sync dispatcher blocks
    /// on `media_alias`, the async one awaits the same function, and the
    /// per-kind gates are reached through `gen_run` rather than
    /// reimplemented. An alias that routed around `gen_denied` /
    /// `video_gate` / `audio_gate` would be a way to spend a media budget
    /// by not spelling the command `/gen`.
    #[test]
    fn both_dispatchers_serve_media_aliases() {
        let src = include_str!("commands.rs");
        let cut = |marker: &str| -> String {
            let body = src.split(marker).nth(1).unwrap_or_else(|| {
                panic!("{marker} not found — this test has drifted from its source")
            });
            let end = body.find("\n}\n").expect("end of function");
            body[..end].to_string()
        };
        let sync = cut("pub fn dispatch(");
        let asy = cut("pub async fn dispatch_async(");

        assert!(
            sync.contains("media_alias(ctx, head, rest)"),
            "the sync dispatcher must reach the alias handler, or the REPL and \
             the TUI silently have no media aliases"
        );
        assert!(
            asy.contains("media_alias(ctx, head, rest)"),
            "the async dispatcher must reach the SAME handler — a second \
             implementation is how the two drift"
        );
        assert!(
            sync.contains("alias_gen_kind(&ctx.agent.cfg, head).is_some()"),
            "the sync arm must be GUARDED by the cheap lookup; without it every \
             unknown word builds a tokio runtime before finding out it is not an alias"
        );

        // The gates live in one place and the alias goes through it.
        let handler = cut("async fn media_alias(");
        assert!(
            handler.contains("gen_denied(actx.principal)"),
            "the media floor must be checked before anything is built: {handler}"
        );
        assert!(
            handler.contains("gen_run(&actx, req)"),
            "and the per-kind gates and rate brakes must come from gen_run, not \
             from a copy: {handler}"
        );
        assert!(
            !handler.contains("video_gate") && !handler.contains("audio_gate"),
            "the alias must not reimplement the per-kind gates — gen_run owns them"
        );
    }

    /// The dispatcher's arms, DERIVED from the source rather than listed
    /// beside it.
    ///
    /// This is the property `config_surface::knobs` has and commands did
    /// not. `knobs()` walks `VelesConfig::default()` through serde, so a
    /// config key cannot be added anywhere without appearing on the
    /// surface that audits it — the inventory is a consequence of the
    /// code and never a second copy of it. `SHARED_COMMANDS` is
    /// hand-written, and every test below checks names *against* it, so
    /// a command that was simply never written down was checked by
    /// nothing whatever.
    ///
    /// Measured, not assumed: on 2026-09-11 a throwaway `"/xyzzy" => …`
    /// arm was added to `dispatch` and the full suite — 1341 tests —
    /// stayed green, with the command live on the REPL, the TUI, the
    /// desktop API and the gateway (and through it IRC and Telegram),
    /// `builtin_cap` answering `None` — which the gateway reads as "no
    /// gate", so any sender who may chat could run it — and no help row
    /// on any surface. That is the same shape as the thirty-one
    /// undeclared duplicates `no_undeclared_command_is_implemented_twice`
    /// was written for, one layer up: it polices the surfaces and never
    /// looked at the dispatcher they all share.
    ///
    /// One group per arm, because `"/mech" | "/mechanisms" =>` is ONE
    /// command with two spellings. That is what lets the help check say
    /// "documented under any of its names" without the hand-written
    /// alias table it used to carry — a table that had already missed
    /// `/tcladmin` joining the `/tcl` arm.
    ///
    /// Crude in the safe direction, the same trade
    /// `no_undeclared_command_is_implemented_twice` states: any `"/name"`
    /// literal left of a `=>` counts. A false positive costs one line in
    /// `SHARED_COMMANDS`; a false negative reopens the hole.
    fn dispatch_arms() -> Vec<Vec<String>> {
        let src = include_str!("commands.rs");
        let body = src
            .split("pub fn dispatch(")
            .nth(1)
            .expect("the shared dispatcher");
        let end = body.find("\n}\n").expect("the end of the dispatcher");
        body[..end]
            .lines()
            .filter_map(|l| l.split_once("=>"))
            .map(|(head, _)| slash_literals(head))
            .filter(|names: &Vec<String>| !names.is_empty())
            .collect()
    }

    /// An arm of `dispatch` IS a command on four surfaces at once. It
    /// must therefore be declared, and being declared is what earns it
    /// the rest of the wiring: `every_shared_command_has_an_acl_tier`
    /// gives it a tier, `every_surface_help_names_every_shared_command`
    /// gives it a help row, and from that row Telegram's `set_my_commands`
    /// menu, the desktop's `/api/commands` table and the TUI's palette
    /// all follow — each of which derives from `help_entries` and so
    /// cannot be forgotten separately.
    ///
    /// So this test is short on purpose. It closes the one link in that
    /// chain that nothing held: the first one.
    #[test]
    fn every_dispatcher_arm_is_declared_and_tiered() {
        let arms = dispatch_arms();
        // A derivation that silently reads nothing is worse than no
        // derivation: it is a green test that has stopped looking.
        // `dispatch` has had forty-odd arms since RM4.3 folded thirty in.
        assert!(
            arms.len() > 40,
            "the scan found only {} arms — it is no longer reading `dispatch`, \
             so every check built on it is passing vacuously",
            arms.len()
        );
        for name in arms.iter().flatten() {
            assert!(
                SHARED_COMMANDS.contains(&name.as_str()),
                "`{name}` is an arm of the shared dispatcher, so it is already live \
                 on the REPL, the TUI, the desktop and the gateway — but it is in no \
                 list. Add it to SHARED_COMMANDS: that is what gives it an ACL tier, \
                 a help row, a Telegram menu entry and a desktop palette row."
            );
            let bare = name.trim_start_matches('/');
            assert!(
                veles_core::acl::builtin_cap(bare).is_some(),
                "`{name}` is dispatched but `acl::builtin_cap` says it is not a \
                 builtin, which the gateway reads as 'no gate' — anyone who may \
                 chat can run it"
            );
        }
    }

    /// And the other direction: a name declared shared with no arm to
    /// answer it. Every surface calls the dispatcher, so such a name is
    /// advertised in four help tables and answers "unknown command"
    /// everywhere.
    #[test]
    fn every_shared_command_has_a_dispatcher_arm() {
        let implemented: std::collections::BTreeSet<String> =
            dispatch_arms().into_iter().flatten().collect();
        let missing: Vec<&&str> = SHARED_COMMANDS
            .iter()
            .filter(|c| !implemented.contains(**c))
            .collect();
        assert!(
            missing.is_empty(),
            "declared in SHARED_COMMANDS but no arm of `dispatch` answers them: \
             {missing:?} — every surface routes through the dispatcher, so these \
             are advertised in four help tables and unknown on all four"
        );
    }

    /// The gateway answers a handful of names from its own table, before
    /// the shared dispatch. That is legitimate — `GATEWAY_OVERRIDES` is
    /// the written form of it — but the table is a second dispatcher,
    /// and a name that appears only there is a command ONE surface has.
    ///
    /// `/version` was exactly that from the beginning: a tier in
    /// `acl.rs`, an answer on the gateway (so IRC and Telegram reached
    /// it, because `known_command` consults `builtin_cap`), and nothing
    /// at all on the REPL, the TUI or the desktop, in no taxonomy list
    /// and in no help table. Found by this test.
    #[test]
    fn every_gateway_builtin_is_declared() {
        let gateway = include_str!("gateway.rs");
        let table = gateway
            .split("let reply = match cmd {")
            .nth(1)
            .expect("the gateway's builtin table");
        let end = table
            .find("\n            };")
            .expect("the end of the gateway's builtin table");
        // The gateway's table matches on the BARE name — the `/` is
        // stripped before it — so the literals carry no slash.
        for line in table[..end].lines() {
            let Some((head, _)) = line.split_once("=>") else {
                continue;
            };
            for bare in quoted_names(head) {
                let name = format!("/{bare}");
                assert!(
                    GATEWAY_OVERRIDES.contains(&name.as_str())
                        || DUPLICATED_ACROSS_SURFACES.contains(&name.as_str()),
                    "the gateway answers `{name}` from its own table but declares it \
                     nowhere. Either it overrides a shared command on purpose (say so \
                     in GATEWAY_OVERRIDES) or it is a command only this surface has — \
                     which is what commands.rs exists to end: move it into `dispatch`."
                );
            }
        }
    }

    /// `/version` answers on a surface that is not the gateway.
    ///
    /// The structural tests above say it is declared, tiered and
    /// documented; this says it actually replies. It could not have,
    /// before: the only implementation lived in the gateway's own
    /// builtin table, so the REPL, the TUI and the desktop answered
    /// "unknown command" while IRC and Telegram answered it fine.
    #[test]
    fn version_answers_off_the_gateway_too() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "version-cmd");
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Repl,
            history: None,
            turn_live: false,
            principal: None,
        };
        let out = dispatch(&mut ctx, "/version", "").expect("the REPL answers /version");
        assert_eq!(out.text, format!("veles {}", env!("CARGO_PKG_VERSION")));
    }

    /// `/crons` is canonical and `/cron` is its alias, so all five
    /// list-shaped commands are plural (`/tasks /subagents /shells
    /// /monitors /crons`). Both spellings must answer IDENTICALLY —
    /// the listing is the same store call, and an alias that drifts
    /// from its canonical form is two commands wearing one name.
    #[test]
    fn crons_and_cron_answer_identically() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "crons-cmd");
        let via_crons = {
            let mut ctx = CommandCtx {
                agent: &agent,
                surface: Surface::Repl,
                history: None,
                turn_live: false,
                principal: None,
            };
            dispatch(&mut ctx, "/crons", "").expect("/crons answers")
        };
        let via_cron = {
            let mut ctx = CommandCtx {
                agent: &agent,
                surface: Surface::Repl,
                history: None,
                turn_live: false,
                principal: None,
            };
            dispatch(&mut ctx, "/cron", "").expect("/cron answers as the alias")
        };
        assert_eq!(
            via_crons.text, via_cron.text,
            "the alias and the canonical form are the same command"
        );
        // And the usage text the canonical form prints names itself.
        let usage = {
            let mut ctx = CommandCtx {
                agent: &agent,
                surface: Surface::Repl,
                history: None,
                turn_live: false,
                principal: None,
            };
            dispatch(&mut ctx, "/crons", "bogus-sub")
        };
        let usage = usage.expect("an unknown sub answers, not an error");
        assert!(
            usage.text.contains("usage: /crons"),
            "the usage text must name the canonical spelling: {}",
            usage.text
        );
    }

    /// `/plan <verb>` is the human's half of the plan read-back, and the
    /// renderer is the TOOL's — the claim this pins, because two
    /// spellings of "what was the plan" drift, and the drift is invisible
    /// from inside either one. It also pins that the mode words still MOVE
    /// and are never eaten by a verb (`/plan off` turning into a read is
    /// the failure where someone believes they left plan mode).
    fn catalog(loc: &str, key: &str) -> String {
        veles_core::i18n::translate(loc, key).unwrap_or_else(|| key.to_string())
    }

    /// `/locale <id>` on a gateway-served surface sets THIS SESSION's
    /// language — stored, followed by the session's own command output,
    /// and without touching another session. The first version switched
    /// the gateway PROCESS, i.e. every IRC, Telegram and desktop user at
    /// once, while the ACL comment said "this session".
    #[test]
    fn locale_on_the_gateway_is_per_session() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let a = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "loc-a");
        let b = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "loc-b");
        let alice = veles_core::acl::Principal::known("irc:alice", veles_core::acl::Role::Trusted);
        let mut ca = CommandCtx {
            agent: &a,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&alice),
        };
        let reply = dispatch(&mut ca, "/locale", "de").expect("claimed").text;
        assert_eq!(
            a.store
                .lock_ok()
                .session_locale_get("loc-a")
                .unwrap()
                .as_deref(),
            Some("de"),
            "stored for the session: {reply}"
        );
        // A's own command output now speaks German. (The probe is the
        // no-catalog refusal: deterministic, translated, and a lookup —
        // unlike /shells, whose listing depends on process-global tasks.)
        let key = "no catalog for '%{arg}' — shipped: %{have}";
        let head = |loc: &str| {
            catalog(loc, key)
                .split("%{arg}")
                .next()
                .unwrap()
                .to_string()
        };
        let out_a = dispatch(&mut ca, "/locale", "zz").expect("claimed").text;
        assert!(out_a.starts_with(&head("de")), "{out_a}");
        // …and wins over a request scope (the desktop window's language).
        {
            let _desk = veles_core::i18n::ScopeGuard::enter(Some("ru".into()));
            let again = dispatch(&mut ca, "/locale", "zz").expect("claimed").text;
            assert!(again.starts_with(&head("de")), "{again}");
        }
        // B has no language of its own: it follows the request scope.
        let mut cb = CommandCtx {
            agent: &b,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&alice),
        };
        {
            let _desk = veles_core::i18n::ScopeGuard::enter(Some("ru".into()));
            let out_b = dispatch(&mut cb, "/locale", "zz").expect("claimed").text;
            assert!(out_b.starts_with(&head("ru")), "{out_b}");
        }
        assert_eq!(b.store.lock_ok().session_locale_get("loc-b").unwrap(), None);

        // reset: A follows the default again.
        dispatch(&mut ca, "/locale", "reset").expect("claimed");
        assert_eq!(a.store.lock_ok().session_locale_get("loc-a").unwrap(), None);
    }

    /// Writing config.toml — and on the gateway, the default for EVERY
    /// user — is an admin act. A trusted IRC user could do both before.
    #[test]
    fn locale_primary_needs_cmd_admin() {
        let (home, _env) = veles_core::testenv::scratch_home();
        let a = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "loc-adm");
        let alice = veles_core::acl::Principal::known("irc:alice", veles_core::acl::Role::Trusted);
        let mut ctx = CommandCtx {
            agent: &a,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&alice),
        };
        for verb in ["primary de", "secondary de"] {
            let out = dispatch(&mut ctx, "/locale", verb).expect("claimed").text;
            assert!(out.contains("cmd-admin"), "{verb}: {out}");
        }
        let cfg = std::fs::read_to_string(home.path().join("config.toml")).unwrap_or_default();
        assert!(!cfg.contains("locale"), "nothing written: {cfg}");
    }

    /// With no swap target, `/locale swap` names the COMMAND that sets
    /// one — not a config key to go and edit by hand (the first version
    /// said "set tui.locale_secondary").
    #[test]
    fn locale_swap_without_a_target_names_the_command() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let a = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "loc-swap");
        let mut ctx = CommandCtx {
            agent: &a,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: None,
        };
        let _en = veles_core::i18n::ScopeGuard::enter(Some("en".into()));
        let out = dispatch(&mut ctx, "/locale", "swap").expect("claimed").text;
        assert!(out.contains("/locale secondary"), "{out}");
        assert!(!out.contains("tui.locale_secondary"), "{out}");
    }

    #[tokio::test]
    async fn plan_read_verbs_share_the_tools_renderer() {
        use veles_core::plan::{Mode, Verb, PLAN_TEXT_PREFIX};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0/unused", "plan-read");
        let sid = agent.session_id.clone();
        {
            let s = agent.store.lock_ok();
            s.meta_set(&format!("{PLAN_TEXT_PREFIX}{sid}"), "STEP ONE: dig")
                .unwrap();
            s.todos_replace(
                &sid,
                &[veles_core::store::TodoItem {
                    id: "a".into(),
                    content: "STEP TWO: ship".into(),
                    status: "pending".into(),
                }],
            )
            .unwrap();
        }
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: None,
        };

        let show = dispatch(&mut ctx, "/plan", "show").expect("claim").text;
        assert!(show.contains("STEP ONE: dig"), "{show}");
        assert!(!show.contains("STEP TWO"), "show is the plan: {show}");
        let tasks = dispatch(&mut ctx, "/plan", "tasks").expect("claim").text;
        assert!(tasks.contains("STEP TWO: ship"), "{tasks}");
        assert!(!tasks.contains("STEP ONE"), "tasks is the list: {tasks}");
        let status = dispatch(&mut ctx, "/plan", "status").expect("claim").text;
        assert!(
            status.contains("STEP ONE") && status.contains("STEP TWO"),
            "{status}"
        );

        // Same characters the tool answers — one renderer, two doors.
        use veles_core::tools::Tool as _;
        let tool_out = veles_core::plan::ShowPlanTool {
            store: agent.store.clone(),
            session_id: sid.clone(),
        }
        .call(serde_json::json!({"action": "plan"}))
        .await;
        let direct = veles_core::plan::render_state(&agent.store.lock_ok(), &sid, &sid, Verb::Show);
        assert!(
            tool_out.contains(&direct),
            "the tool and the renderer must agree:\n{tool_out}\n---\n{direct}"
        );

        // The mode words still MOVE, and were not touched by any of the
        // reads above.
        assert_eq!(
            veles_core::plan::mode_for(&agent.store.lock_ok(), &sid),
            Mode::Act
        );
        let on = dispatch(&mut ctx, "/plan", "on").expect("claim").text;
        assert!(on.contains("plan mode ON"), "{on}");
        let off = dispatch(&mut ctx, "/plan", "off").expect("claim").text;
        assert!(off.contains("plan mode off"), "{off}");
        // `/plan plan` is the MODE word (`Mode` claims it), not a read —
        // the tables are disjoint where the command door meets them.
        let plan = dispatch(&mut ctx, "/plan", "plan").expect("claim").text;
        assert!(plan.contains("plan mode ON"), "{plan}");
        dispatch(&mut ctx, "/plan", "off");
        // A bare `/plan` reports; a nonsense argument is the mode path's
        // error, not a silent read.
        let bare = dispatch(&mut ctx, "/plan", "").expect("claim").text;
        assert!(bare.contains("plan mode is off"), "{bare}");
        let junk = dispatch(&mut ctx, "/plan", "sideways").expect("claim").text;
        assert!(junk.contains("unknown plan mode"), "{junk}");
    }

    /// The gateway's builtin table runs BEFORE the shared dispatch, which
    /// is the only reason an override overrides. If that order is ever
    /// swapped the overrides become dead code and the sender-scoped /acl
    /// silently turns into the owner-surface one.
    #[test]
    fn the_gateway_answers_its_own_builtins_first() {
        let gateway = include_str!("gateway.rs");
        let builtins = gateway
            .find("let reply = match cmd {")
            .expect("the gateway's builtin table");
        let shared = gateway
            .find("commands::dispatch")
            .expect("the gateway's shared dispatch");
        assert!(
            builtins < shared,
            "the shared dispatch now runs before the builtin table — every \
             GATEWAY_OVERRIDES entry just became unreachable"
        );
    }

    /// A shared command must be answerable WHILE a turn is running.
    ///
    /// This is the `/steer` hang, generalized. `run_turn` takes
    /// `&self`, but every surface holds the agent behind an exclusive
    /// lock and the turn holds that lock for its whole life
    /// (`tui.rs`'s `#[allow(clippy::await_holding_lock)]` says so out
    /// loud). So a command typed mid-turn does not queue politely — on
    /// the TUI it blocks the single-threaded UI loop, freezing the
    /// terminal until the turn it was trying to influence has already
    /// finished.
    ///
    /// The fix is not a timeout or a `try_lock`. It is that answering
    /// "what is this session's plan mode" never needed exclusive
    /// access to the agent in the first place.
    #[test]
    fn a_shared_command_answers_while_a_turn_is_running() {
        use std::sync::Arc;
        use std::time::Duration;

        let (_home, _env) = veles_core::testenv::scratch_home();
        let (url, release) = veles_core::testenv::stalling_provider();
        let agent = Arc::new(veles_core::testenv::mock_agent(&url, "mid-turn-dispatch"));

        // The turn: a real one, on its own thread with its own runtime
        // — the TUI's shape exactly.
        let turn_agent = agent.clone();
        let turn = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("rt");
            rt.block_on(async {
                let mut history = Vec::new();
                let _ = turn_agent
                    .run_turn(&mut history, "hello", None, |_| {})
                    .await;
            });
        });

        // Let it reach the socket and stall there.
        std::thread::sleep(Duration::from_millis(400));

        let (tx, rx) = std::sync::mpsc::channel();
        let cmd_agent = agent.clone();
        std::thread::spawn(move || {
            let mut ctx = CommandCtx {
                agent: &cmd_agent,
                surface: Surface::Repl,
                history: None,
                turn_live: true,
                principal: None,
            };
            let _ = tx.send(dispatch(&mut ctx, "/plan", ""));
        });

        let answered = rx.recv_timeout(Duration::from_secs(3));
        let _ = release.send(());
        let _ = turn.join();

        assert!(
            answered.is_ok(),
            "`/plan` did not answer within 3s while a turn was in flight — \
             the turn still holds the agent exclusively, so every command \
             typed mid-turn blocks behind it (and on the TUI, freezes the \
             whole frontend)"
        );
        let answered = answered.expect("answered");
        assert!(
            answered.is_some(),
            "the shared dispatcher must claim `/plan`"
        );
        assert!(
            answered.expect("handled").text.contains("plan"),
            "the answer must be the command's text, not an error"
        );
    }

    /// `CommandCtx.principal` was populated at the gateway and read by
    /// NOTHING: shared-command tiering rested entirely on
    /// `builtin_cap`, one tier per NAME. So `/acl` had to be
    /// cmd-admin — locking the self-report away from the caller it is
    /// about — or open, which would have opened `grant` too.
    #[test]
    fn acl_subcommands_are_tiered_inside_the_command() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "acl-cmd");

        let run = |p: Option<&Principal>, rest: &str| -> String {
            acl_command(&AclCtx::from_agent(&agent, p), rest)
        };

        let guest = Principal::known("irc:nobody", Role::Guest);
        let admin = Principal::known("irc:boss", Role::Admin);
        let owner = Principal::known("irc:me", Role::Owner);

        // The bare form is about YOU, at every tier.
        let mine = run(Some(&guest), "");
        assert!(
            mine.contains("guest") && mine.contains("caps:") && mine.contains("ceiling"),
            "a guest learns their role, caps and ceiling: {mine}"
        );

        // Reading the rules is cmd-admin.
        let denial = run(Some(&guest), "list");
        assert!(
            denial.contains("cmd-admin") && denial.contains("guest"),
            "the denial names the cap and the caller: {denial}"
        );
        assert!(
            !run(Some(&admin), "list").contains("[acl]"),
            "an admin reads the rules"
        );

        // Changing them is owner-only — the same floor /pair's
        // management subcommands use, so no surface is the soft way in.
        let denial = run(Some(&admin), "grant irc:x user");
        assert!(
            denial.contains("[acl]") && denial.contains("all"),
            "an admin does not grant: {denial}"
        );
        let granted = run(Some(&owner), "grant irc:x trusted");
        assert!(
            granted.contains("granted") && granted.contains("trusted"),
            "{granted}"
        );
        assert!(
            run(Some(&admin), "list").contains("irc:x"),
            "and the grant is visible in the listing"
        );
        assert!(
            run(Some(&owner), "revoke irc:x").contains("revoked"),
            "owner revokes"
        );

        // An owner SURFACE passes no principal and is the owner.
        assert!(
            !run(None, "list").contains("[acl]"),
            "the human at the keyboard is the owner by definition"
        );
    }

    /// The answer to "why does jimmy have a dunce cap" — the question
    /// specificity precedence creates, since it is no longer about a
    /// line number.
    #[test]
    fn acl_explain_names_the_winner_and_what_it_beat() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        // A REAL config file, in the scratch home: `acl_cfg` reads the
        // LIVE `[acl]` (that is the point of it — `configedit` promises
        // those edits are in effect now), and a test that leaned on the
        // agent's boot snapshot instead would be measuring the wrong
        // path and would read the developer's own config on a
        // configured machine.
        //
        // Declaration order is DELIBERATELY wrong: the broad rule
        // first, the narrow one after, which under the old
        // first-match-in-config-order resolution would have made
        // everybody a user.
        let dir = veles_core::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(
            dir.join("config.toml"),
            "[acl]\nowner = [\"irc:me\"]\n\n\
             [[acl.entries]]\nidentity = \"*!*@*.example\"\nrole = \"user\"\n\n\
             [[acl.entries]]\nidentity = \"jimmy!asshole@444.example\"\nrole = \"guest\"\n",
        )
        .expect("write config");
        veles_core::acl::invalidate_live_config();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "acl-explain");
        let ctx = AclCtx::from_agent(&agent, None);

        let jimmy = acl_command(&ctx, "explain jimmy!asshole@444.example");
        assert!(
            jimmy.contains("guest"),
            "the narrow rule wins over the broad one: {jimmy}"
        );
        assert!(
            jimmy.contains("*!*@*.example"),
            "and the runner-up it beat is named: {jimmy}"
        );
        assert!(
            jimmy.contains("specific"),
            "the report says WHY, not just what: {jimmy}"
        );

        let stranger = acl_command(&ctx, "explain someone!else@host.example");
        assert!(
            stranger.contains("user"),
            "everyone else still gets the broad rule: {stranger}"
        );
        assert!(
            acl_command(&ctx, "explain").starts_with("usage: /acl explain"),
            "the bare form still explains itself"
        );
    }

    /// USER, 2026-09-16: *"is it still resolving people as guests on
    /// telegram?"* — `/acl explain` is the command that should have
    /// answered that, and on a connector with a shut door the role alone
    /// is a half-truth: it reads as "they get a guest turn", which is the
    /// exact misreading the door exists to prevent.
    #[test]
    fn acl_explain_says_when_a_connector_admits_this_sender_not_at_all() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let dir = veles_core::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(
            dir.join("config.toml"),
            "[acl]\nowner = [\"telegram:1\"]\ndefault_role = \"guest\"\n\n\
             [acl.strangers]\ntelegram = \"ignore\"\n",
        )
        .expect("write config");
        veles_core::acl::invalidate_live_config();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "acl-explain-door");
        let ctx = AclCtx::from_agent(&agent, None);

        let scoped = acl_command(&ctx, "explain telegram:111222333 --platform telegram");
        assert!(
            scoped.contains("guest"),
            "the role the ACL would give is still reported: {scoped}"
        );
        assert!(
            scoped.contains("never reaches a turn"),
            "and so is the door in front of it: {scoped}"
        );
        assert!(
            scoped.contains("[[acl.users]]"),
            "…and how to let somebody in: {scoped}"
        );

        // The same sender asked about without a connector: the door is
        // not asserted where it was not asked about, but its existence is
        // — a bare answer that omitted it would be the half-truth again.
        let bare = acl_command(&ctx, "explain telegram:111222333");
        assert!(
            !bare.contains("never reaches a turn"),
            "an unscoped question gets an unscoped answer: {bare}"
        );
        assert!(
            bare.contains("--platform"),
            "…that points at where the rest of the answer is: {bare}"
        );
    }

    /// The flag form and the positional form are the SAME question.
    ///
    /// `/acl grant` takes `--network n` while `explain` took positionals
    /// only, so `/acl explain bob --network libera` read the flag as the
    /// NAME of a network and answered "no rule matched" — plausible,
    /// confident, and about a question nobody asked. It cost two wrong
    /// live verifications of `/acl promote` before anybody looked at the
    /// invocation instead of the answer.
    #[test]
    fn acl_explain_takes_the_flags_the_command_beside_it_takes() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let dir = veles_core::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        std::fs::write(
            dir.join("config.toml"),
            "[acl]\nowner = [\"irc:me\"]\ndefault_role = \"guest\"\n\n\
             [[acl.entries]]\nidentity = \"bob!*@*\"\nrole = \"user\"\nnetwork = \"libera\"\n",
        )
        .expect("write config");
        veles_core::acl::invalidate_live_config();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "acl-explain-flags");
        let ctx = AclCtx::from_agent(&agent, None);

        let positional = acl_command(&ctx, "explain bob!b@h libera");
        assert!(
            positional.contains("network libera"),
            "the scoped entry is reachable when the network is positional: {positional}"
        );
        assert_eq!(
            acl_command(&ctx, "explain bob!b@h --network libera"),
            positional,
            "the flag form asks the same question and gets the same answer"
        );
        assert_eq!(
            acl_command(&ctx, "explain bob!b@h --net libera"),
            positional,
            "and so does the short one"
        );
        assert_eq!(
            acl_command(&ctx, "explain bob!b@h --network libera --chan #x"),
            acl_command(&ctx, "explain bob!b@h libera #x"),
            "channel is a flag or a positional, not a coin toss"
        );

        // The failure named: a flag in a value slot must never be read
        // as the value.
        let unknown = acl_command(&ctx, "explain bob!b@h --nope libera");
        assert!(
            unknown.contains("unknown flag"),
            "an unrecognised flag is refused: {unknown}"
        );
        assert!(
            !unknown.contains("no rule matched"),
            "...and it is NOT quietly used as a network name: {unknown}"
        );
        assert!(
            acl_command(&ctx, "explain bob!b@h --network").contains("needs a value"),
            "a flag with nothing after it is refused too"
        );
        assert!(
            acl_command(&ctx, "explain --network libera").contains("is a flag, not an identity"),
            "so is a flag where the identity belongs"
        );
        assert!(
            acl_command(&ctx, "explain bob!b@h libera #x plat extra")
                .contains("nothing to do with"),
            "and a spare argument is not dropped on the floor"
        );
    }

    /// `/sandbox` — the named boxes, manageable from a chat line
    /// rather than only from config.toml (USER: "full first-class
    /// support in all frontends").
    ///
    /// `where` is open, everything that declares or assigns is
    /// cmd-admin, and `use` is additionally checked against the
    /// caller's REACHABLE SET — so naming a box is never a way into
    /// one.
    #[tokio::test]
    async fn sandbox_management_is_tiered_and_the_names_are_checked() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "sandbox-cmd");
        let user = Principal::known("irc:friend", Role::User);
        let admin = Principal::known("irc:boss", Role::Admin);

        // A caller is told which box their own files are in. Being
        // told is what keeps them from assuming they are on the host.
        veles_core::sandboxes::set_session_sandbox(&agent.session_id, "chan:irc/net/#veles");
        veles_core::acl::set_session_principal(&agent.session_id, user.clone());
        let where_ = sandbox_command(&AclCtx::from_agent(&agent, Some(&user)), "where").await;
        assert!(
            where_.contains("chan:irc/net/#veles"),
            "the answer names the box: {where_}"
        );

        // Declaring and assigning are admin.
        for cmd in [
            "create quarantine",
            "assign quarantine --role guest",
            "list",
        ] {
            let d = sandbox_command(&AclCtx::from_agent(&agent, Some(&user)), cmd).await;
            assert!(d.contains("cmd-admin"), "`{cmd}` must be admin-gated: {d}");
        }
        let made = sandbox_command(
            &AclCtx::from_agent(&agent, Some(&admin)),
            "create quarantine --note keep it away",
        )
        .await;
        assert!(made.contains("quarantine"), "{made}");
        let bound = sandbox_command(
            &AclCtx::from_agent(&agent, Some(&admin)),
            "assign quarantine --role guest --network net",
        )
        .await;
        assert!(
            bound.contains("role guest") && bound.contains("network net"),
            "the assignment echoes its selectors: {bound}"
        );
        let listed = sandbox_command(&AclCtx::from_agent(&agent, Some(&admin)), "list").await;
        assert!(
            listed.contains("quarantine") && listed.contains("keep it away"),
            "the declaration and its note are in the listing: {listed}"
        );
        assert!(
            listed.contains("admin default zone"),
            "and the posture, which is the thing an admin most needs to \
             see before opting out: {listed}"
        );

        // A bind with no selectors would capture everyone — refused at
        // the store, and the refusal reaches the caller.
        let oops = sandbox_command(
            &AclCtx::from_agent(&agent, Some(&admin)),
            "assign quarantine",
        )
        .await;
        assert!(oops.contains("selectors"), "{oops}");

        // `use` is checked against the reachable set, not against the
        // name being spellable.
        veles_core::sandboxes::set_session_reachable(&user.subject, vec!["shared".into()]);
        let denied =
            sandbox_command(&AclCtx::from_agent(&agent, Some(&user)), "use quarantine").await;
        assert!(
            denied.contains("quarantine"),
            "a name outside the set is a denial that names it: {denied}"
        );
        assert!(
            !denied.contains("now run"),
            "and it does not silently succeed: {denied}"
        );
        let ok = sandbox_command(&AclCtx::from_agent(&agent, Some(&user)), "use shared").await;
        assert!(ok.contains("shared") && ok.contains("now run"), "{ok}");
        assert_eq!(
            veles_core::sandboxes::session_sandbox(&agent.session_id).as_deref(),
            Some("shared"),
            "the override has to actually land where the tool layer reads it"
        );

        // An admin reaches anything that exists.
        assert!(
            sandbox_command(&AclCtx::from_agent(&agent, Some(&admin)), "use quarantine")
                .await
                .contains("now run"),
            "admins reach every box"
        );

        // Dropping the declaration is not destroying the workspace,
        // and the answer says so — the two are separate decisions
        // because a workspace holds someone's files.
        let dropped =
            sandbox_command(&AclCtx::from_agent(&agent, Some(&admin)), "drop quarantine").await;
        assert!(
            dropped.contains("untouched"),
            "forgetting a declaration must not read as a purge: {dropped}"
        );
    }

    /// `/limits` reads open and writes at cmd-admin, and the write is
    /// live — which is the whole difference between a manageable
    /// ceiling and a config comment.
    #[test]
    fn limits_read_open_and_set_admin_and_the_set_is_live() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "limits-cmd");
        let run = |p: Option<&Principal>, rest: &str| -> String {
            limits_command(&AclCtx::from_agent(&agent, p), rest)
        };
        let guest = Principal::known("irc:nobody", Role::Guest);
        let admin = Principal::known("irc:boss", Role::Admin);

        let shown = run(Some(&guest), "");
        assert!(
            shown.contains("150k") && shown.contains("250k") && shown.contains("unlimited"),
            "the whole table, with the guest and non-admin numbers the user asked for: {shown}"
        );
        // CHURN, 2026-09-09: the "yours" row used to be the ceiling
        // alone. A ceiling with no reading against it does not answer
        // "how much have I got left", so it now carries the caller's
        // own spend and turn count as well.
        assert!(
            shown.contains("yours (irc:nobody)") && shown.contains("context 150k"),
            "and the caller's own numbers: {shown}"
        );
        assert!(
            shown.contains("spent 0 of") && shown.contains("turns this hour"),
            "…including what they have actually used: {shown}"
        );
        // All three budgets are on the table, each saying what it is
        // for — tightening the wrong one is the failure mode here.
        assert!(
            shown.contains("tokens per day") && shown.contains("turns per hour"),
            "every budget is shown, not just the context ceiling: {shown}"
        );
        assert!(
            shown.contains("concurrent turns per caller"),
            "…including the concurrency cap: {shown}"
        );

        assert!(
            run(Some(&guest), "set guest 999").contains("cmd-admin"),
            "a guest does not raise their own ceiling"
        );
        let set = run(Some(&admin), "set guest 90000");
        assert!(set.contains("90k") && set.contains("live"), "{set}");
        assert!(
            run(Some(&guest), "").contains("guest    90k"),
            "the new number is what every surface now reads: {}",
            run(Some(&guest), "")
        );
        assert!(
            run(Some(&admin), "clear guest").contains("configured value"),
            "and it can be handed back to config"
        );
        assert!(
            run(Some(&guest), "").contains("150k"),
            "back to the default"
        );

        // The other two budgets set and clear through the same verb,
        // and the value is live for the reader — the whole point of the
        // runtime override.
        let set = run(Some(&admin), "set tokens-per-day guest 40000");
        assert!(set.contains("40k") && set.contains("live"), "{set}");
        assert!(
            run(Some(&guest), "").contains("guest    40k"),
            "the tokens/day table shows it: {}",
            run(Some(&guest), "")
        );
        let set = run(Some(&admin), "set turns-per-hour guest 7");
        assert!(
            set.contains('7') && !set.contains("0k"),
            "turns are not kilo-anything: {set}"
        );
        assert!(run(Some(&guest), "").contains("guest    7"));
        // Concurrency is one number, so both spellings must agree.
        assert!(run(Some(&admin), "set concurrent-turns 5").contains('5'));
        assert!(run(Some(&guest), "").contains("concurrent turns per caller · 5"));
        assert!(run(Some(&admin), "set concurrent-turns default 6").contains('6'));
        assert!(run(Some(&guest), "").contains("concurrent turns per caller · 6"));
        // A guest raises none of them.
        for attempt in [
            "set tokens-per-day guest 999999",
            "set turns-per-hour guest 999",
            "set concurrent-turns 99",
        ] {
            assert!(
                run(Some(&guest), attempt).contains("cmd-admin"),
                "a guest must not set {attempt:?}"
            );
        }
        // …and each clears back to config independently.
        assert!(run(Some(&admin), "clear tokens-per-day guest").contains("configured value"));
        assert!(
            run(Some(&guest), "").contains("guest    7"),
            "clearing tokens/day must not clear turns/hour: {}",
            run(Some(&guest), "")
        );
    }

    /// `/usage` is per-caller: a guest reads their own budgets and
    /// spend, a guest may NOT read someone else's bill, and an admin
    /// reads any subject. The windows are the enforcement windows, so
    /// the readings are the ones the gates will refuse on.
    #[test]
    fn usage_is_per_caller_and_admin_only_for_other_subjects() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "usage-quotas");
        let run = |p: Option<&Principal>, rest: &str| -> String {
            usage_quotas_command(&AclCtx::from_agent(&agent, p), rest)
        };
        let guest = Principal::known("irc:nobody", Role::Guest);
        let admin = Principal::known("irc:boss", Role::Admin);

        // Own report: every budget line, the rolling-24h spend split,
        // and the windows named.
        let own = run(Some(&guest), "");
        assert!(own.contains("usage for irc:nobody"), "{own}");
        assert!(own.contains("tokens/day"), "{own}");
        assert!(own.contains("turns/hour"), "{own}");
        assert!(own.contains("media/hour"), "{own}");
        assert!(own.contains("video/hour"), "{own}");
        assert!(own.contains("eval/hour"), "{own}");
        assert!(own.contains("context ceiling"), "{own}");
        assert!(
            own.contains("rolling 24h") && own.contains("rolling hour"),
            "{own}"
        );
        assert!(
            own.contains("cached/6"),
            "the discount is shown, not hidden: {own}"
        );

        // A guest asking about someone else is refused at cmd-admin:
        // one person's bill is not another guest's business.
        assert!(
            run(Some(&guest), "irc:someoneelse").contains("cmd-admin"),
            "a guest may not read another subject's usage"
        );
        // An admin may.
        let other = run(Some(&admin), "irc:someoneelse");
        assert!(other.contains("usage for irc:someoneelse"), "{other}");

        // A zero spend reads "0", not "unlimited" — a reading is not a
        // ceiling ("unlimited billed" was the bug).
        assert!(
            own.contains("tokens/day: 0 billed"),
            "a zero reading prints 0, not unlimited: {own}"
        );
    }

    /// A mask-shaped subject argument resolves through the mask ENTRIES,
    /// so `a`'s 50M per-subject override applies and the role reads
    /// `user`, not guest. The budget_key (`ident@host`) spelling lands on
    /// the same ledger the spend is counted against.
    #[test]
    fn usage_of_a_mask_subject_uses_the_mask_entry_and_its_override() {
        use veles_core::acl::{Principal, Role, SubjectBudgets};
        let (_home, _env) = veles_core::testenv::scratch_home();
        // Build the acl directly (the agent's cfg is behind an Arc): a's
        // network-scoped mask entry + a 50M subject cap.
        let mut acl: veles_core::acl::AclConfig = Default::default();
        acl.entries.push(veles_core::acl::AclEntry {
            identity: "*!~a@86B64C8E:81F44426:DD62833D:IP".into(),
            network: Some("irc.synirc.net".into()),
            channel: None,
            platform: None,
            role: "user".into(),
            extra_caps: vec![],
            deny_caps: vec![],
            max_context_tokens: None,
            ticket_role: None,
        });
        acl.limits.by_subject.insert(
            "~a@86b64c8e:81f44426:dd62833d:ip".into(),
            SubjectBudgets {
                tokens_per_day: Some(50_000_000),
                ..Default::default()
            },
        );
        let store = std::sync::Arc::new(std::sync::Mutex::new(
            veles_core::store::Store::open(&_home.path().join("db")).unwrap(),
        ));
        let admin = Principal::known("irc:boss", Role::Admin);
        let ctx = AclCtx {
            store: &store,
            fallback: &acl,
            session_id: "",
            principal: Some(&admin),
            sender: None,
        };
        let out = usage_quotas_command(&ctx, "~a@86b64c8e:81f44426:dd62833d:ip");
        assert!(
            out.contains("(user,"),
            "the mask entry gives user, not guest: {out}"
        );
        assert!(out.contains("per-subject override"), "{out}");
        assert!(out.contains("of 50000k"), "the 50M override ceiling: {out}");
    }

    /// A runtime override an admin set BEFORE the other two budgets
    /// existed is stored as `acl_limit:<role>` with no budget infix.
    /// Reading it as anything but the context ceiling would silently
    /// move someone's limit to a different budget on upgrade.
    #[test]
    fn a_legacy_runtime_override_still_reads_as_the_context_ceiling() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "limits-legacy");
        let store = agent.store.clone();
        store
            .lock()
            .unwrap()
            .meta_set("acl_limit:guest", "12000")
            .unwrap();
        let budgets = veles_core::acl::runtime_budgets(&store);
        assert_eq!(
            budgets,
            vec![(
                veles_core::acl::Budget::Context,
                "guest".to_string(),
                12_000
            )]
        );
        let cfg = veles_core::acl::with_runtime_limits(
            serde_yaml::from_str("owner: [\"irc:o\"]").unwrap(),
            &store,
        );
        assert_eq!(cfg.limits.for_role(veles_core::acl::Role::Guest), 12_000);
        // …and it did not leak into the new budgets.
        assert_eq!(
            cfg.limits
                .tokens_per_day_for_role(veles_core::acl::Role::Guest),
            200_000
        );
    }

    /// The explicit ask verbs honour the ask's BINDING, not just the
    /// capability.
    ///
    /// `Cap::Approve` gets a caller as far as the verb; whose ask it is
    /// decides the rest. Before the sigil fold these verbs went
    /// straight to `ask::resolve()` with the id, so a second holder of
    /// `Approve` in a shared channel could decide someone else's
    /// approval by naming it — the hole S15 closed for a bare "1",
    /// still open under the longer spelling.
    #[test]
    fn the_ask_verbs_are_bound_to_whoever_opened_the_ask() {
        use veles_core::acl::{Principal, Role};
        use veles_core::ask::{AskChoice, AskKind, AskRequest, Decider};
        // The ACL snapshot this test writes is process-global, so it
        // takes the one lock (testenv rule 1): without it a neighbour
        // test's snapshot — or its clearing of this one — is what the
        // owner override below is checked against.
        let _g = veles_core::testenv::guard();
        veles_core::ask::set_acl_snapshot_for_tests(Some(
            serde_yaml::from_str("owner: [\"irc:boss\"]").unwrap(),
        ));
        let chat = veles_core::testenv::chat_key();
        let req = AskRequest::new(
            "s-bound",
            AskKind::Approval,
            "run rm?",
            vec![AskChoice::new("once", "once")],
        );
        let id = req.id.clone();
        let _rx = veles_core::ask::register(&req, Some(&chat), Some("irc:alice"));

        let alice = Principal::known("irc:alice", Role::Trusted);
        let bob = Principal::known("irc:bob", Role::Trusted);
        let guest = Principal::known("irc:nobody", Role::Guest);

        // Listing is a report, open to anyone who can see the channel.
        let listed = ask_command(
            Some(&guest),
            Some(&chat),
            Decider::sender(Some("irc:nobody")),
            "asks",
            "",
        );
        assert!(listed.contains(&id), "the open ask is listed: {listed}");

        // A guest cannot decide anything at all.
        assert!(
            ask_command(
                Some(&guest),
                Some(&chat),
                Decider::sender(Some("irc:nobody")),
                "approve",
                &id,
            )
            .contains("approve"),
            "a guest is refused at the capability"
        );

        // Bob HOLDS Approve and still cannot decide Alice's ask.
        let bobs = ask_command(
            Some(&bob),
            Some(&chat),
            Decider::sender(Some("irc:bob")),
            "approve",
            &id,
        );
        assert!(
            bobs.contains("yours to answer"),
            "the binding must refuse a stranger with the capability: {bobs}"
        );
        assert!(
            !veles_core::ask::open_for_chat(&chat).is_empty(),
            "…and the ask is still open"
        );

        // Alice can.
        let hers = ask_command(
            Some(&alice),
            Some(&chat),
            Decider::sender(Some("irc:alice")),
            "approve",
            &id,
        );
        assert!(hers.contains("recorded"), "{hers}");
        assert!(veles_core::ask::open_for_chat(&chat).is_empty());

        // A usage line rather than a silent no-op.
        assert!(ask_command(
            Some(&alice),
            None,
            Decider::LocalOwner,
            "answer",
            "just-an-id"
        )
        .contains("usage:"));
        veles_core::ask::set_acl_snapshot_for_tests(None);
    }

    /// `/lockdown`: any admin engages it from anywhere, and only a
    /// LOCAL surface lifts it. The surface table is the security
    /// boundary here, so it is pinned per surface rather than trusted
    /// to one representative case.
    #[test]
    fn lockdown_engages_from_anywhere_and_lifts_only_locally() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "lockdown-cmd");
        let run = |p: Option<&Principal>, surface: Surface, rest: &str| -> String {
            lockdown_command(&AclCtx::from_agent(&agent, p), surface, rest)
        };
        let guest = Principal::known("irc:nobody", Role::Guest);
        let admin = Principal::known("irc:boss", Role::Admin);

        // Reading is open: a refused sender may ask what refused them.
        assert!(run(Some(&guest), Surface::Gateway, "").contains("off"));
        // Engaging is cmd-admin, from any surface.
        assert!(
            run(Some(&guest), Surface::Gateway, "on").contains("cmd-admin"),
            "a guest does not get to silence the bot"
        );
        let on = run(Some(&admin), Surface::Gateway, "on hostile channel");
        assert!(on.contains("ENGAGED"), "{on}");
        assert!(
            run(Some(&guest), Surface::Gateway, "").contains("hostile channel"),
            "the reason is part of the answer"
        );

        // Lifting: refused from a connector at ANY role, including the
        // owner, and accepted from every local surface.
        for p in [None, Some(&admin)] {
            let refused = run(p, Surface::Gateway, "off");
            assert!(
                refused.contains("LOCAL"),
                "a connector must not lift it, whoever is asking: {refused}"
            );
            assert!(
                veles_core::lockdown::engaged(Some(&agent.store)),
                "…and it is still engaged"
            );
        }
        for surface in [Surface::Repl, Surface::Tui, Surface::Desktop] {
            veles_core::lockdown::engage(&agent.store, "irc:boss", "again").unwrap();
            let lifted = run(None, surface, "off");
            assert!(
                lifted.contains("lifted"),
                "{surface:?} is local and may lift it: {lifted}"
            );
            assert!(!veles_core::lockdown::engaged(Some(&agent.store)));
        }
        assert!(
            run(None, Surface::Repl, "off").contains("not engaged"),
            "lifting nothing says so rather than claiming success"
        );
        assert!(
            run(None, Surface::Repl, "frobnicate").contains("usage:"),
            "an unknown verb is a usage line, not a silent no-op"
        );
    }

    /// `/queue` lists, edits and cancels the SAME store rows the
    /// desktop endpoints and the TUI composer touch — the one-queue
    /// claim, pinned at the command level (R4.6). Dispatched mid-turn
    /// too: engine-state answers, no lock the running turn holds.
    #[test]
    fn queue_command_sees_edits_and_cancels_the_shared_rows() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "queue-cmd");
        let sid = agent.session_id.clone();
        agent
            .store
            .lock_ok()
            .queue_push(&sid, "hello pending")
            .unwrap();

        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Repl,
            history: None,
            turn_live: true, // mid-turn /queue must answer
            principal: None,
        };
        let out = dispatch(&mut ctx, "/queue", "").expect("claim").text;
        assert!(
            out.contains("pending"),
            "listing shows the row and state: {out}"
        );
        assert!(out.contains("hello"), "{out}");

        let out = dispatch(&mut ctx, "/queue", "edit 1 rewritten text")
            .expect("claim")
            .text;
        assert!(out.contains("rewritten"), "{out}");
        assert_eq!(
            agent.store.lock_ok().queue_items(&sid).unwrap()[0].body,
            "rewritten text",
            "the edit must land in the store, not a session-local copy"
        );

        let out = dispatch(&mut ctx, "/queue", "cancel 1")
            .expect("claim")
            .text;
        assert!(out.contains("cancelled"), "{out}");
        assert!(
            agent.store.lock_ok().queue_items(&sid).unwrap().is_empty(),
            "cancel deletes the row, pending rows only"
        );
    }

    /// The transcript editors refuse while a turn is live — the
    /// deliberate lock-order decision, pinned.
    #[test]
    fn transcript_editers_refuse_while_a_turn_is_live() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "refuse-live");
        let mut hist = Vec::new();
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Repl,
            history: Some(&mut hist),
            turn_live: true,
            principal: None,
        };
        for cmd in ["/clear", "/undo", "/compress", "/compact"] {
            let h = dispatch(&mut ctx, cmd, "").expect(cmd);
            assert!(
                h.text.contains("/cancel"),
                "`{cmd}` must refuse with the honest message, got: {}",
                h.text
            );
            assert!(!h.reload, "a refusal must not trigger a reload");
        }
    }

    /// `/model` with no argument reports; with one, it SETS — the
    /// report-3 fix. The TUI's arm was read-only and IRC/Telegram had
    /// nothing at all.
    #[test]
    fn model_reports_and_sets() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "model-set");
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Tui,
            history: None,
            turn_live: false,
            principal: None,
        };
        let report = dispatch(&mut ctx, "/model", "").expect("report");
        assert!(
            report.text.contains("model:"),
            "report shape: {}",
            report.text
        );
        let set = dispatch(&mut ctx, "/model", "test-model").expect("set");
        assert!(set.text.contains("test-model"), "set ack: {}", set.text);
        let after = dispatch(&mut ctx, "/model", "").expect("re-report");
        assert!(
            after.text.contains("test-model"),
            "the override must now be visible: {}",
            after.text
        );
    }

    /// Bare `/model` is a read, open at the status tier (USER
    /// 2026-10-02: a user on IRC asking which model answered was
    /// refused). Switching shapes the session and keeps `cmd-session`,
    /// which a rule's `extra_caps` can grant one person.
    /// `/configsync` lists the config changes that wait for the owner
    /// and `apply` lands them — through the shared dispatcher, so the
    /// desktop, IRC and Telegram reach it alike. Anyone else is refused.
    #[test]
    fn configsync_lists_and_lands_what_waits_and_is_the_owners() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[model]\ndefault = \"m\"\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
            .unwrap();
        let offer = veles_core::configsync::ConfigSyncOffer {
            from_node: "peer-node".into(),
            from_name: "laptop".into(),
            since_seq: 0,
            rows: vec![veles_core::configsync::ConfigLogRow {
                node: "peer-node".into(),
                node_seq: 1,
                at: veles_core::configsync::now_stamp(),
                key: "acl.default_role".into(),
                value: "\"user\"".into(),
            }],
        };
        veles_core::configsync::inbox_store(
            &veles_core::configsync::inbox_dir(),
            "laptop",
            "t1",
            offer,
        )
        .unwrap();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "m");
        let user = Principal::known("irc:friend", Role::User);
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&user),
        };
        let refused = dispatch(&mut ctx, "/configsync", "apply").expect("refusal");
        assert!(!refused.text.contains("from laptop"), "{}", refused.text);
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("default_role"));

        ctx.principal = None;
        let listed = dispatch(&mut ctx, "/configsync", "").expect("listing");
        assert!(
            listed.text.contains("from laptop: acl.default_role"),
            "{}",
            listed.text
        );
        let landed = dispatch(&mut ctx, "/configsync", "apply").expect("apply");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("default_role = \"user\""),
            "{}",
            landed.text
        );
        assert!(veles_core::configsync::inbox_waiting().unwrap().is_empty());
    }

    /// A held change that is wrong here is set aside, not applied.
    #[test]
    fn configsync_drop_sets_waiting_changes_aside_unapplied() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[sandbox]\nnetwork = false\n").unwrap();
        let offer = veles_core::configsync::ConfigSyncOffer {
            from_node: "peer-node".into(),
            from_name: "vps".into(),
            since_seq: 0,
            rows: vec![veles_core::configsync::ConfigLogRow {
                node: "peer-node".into(),
                node_seq: 1,
                at: veles_core::configsync::now_stamp(),
                key: "sandbox.network".into(),
                value: "true".into(),
            }],
        };
        veles_core::configsync::inbox_store(
            &veles_core::configsync::inbox_dir(),
            "vps",
            "t1",
            offer,
        )
        .unwrap();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "m");
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: None,
        };
        let out = dispatch(&mut ctx, "/configsync", "drop").expect("drop");
        assert!(out.text.contains("dropped 1 offer"), "{}", out.text);
        assert!(veles_core::configsync::inbox_waiting().unwrap().is_empty());
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("network = false"));
    }

    #[test]
    fn a_user_reads_the_model_but_switching_needs_cmd_session() {
        use veles_core::acl::{Cap, Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "model-tier");
        let user = Principal::known("irc:friend", Role::User);
        assert_eq!(veles_core::acl::builtin_cap("model"), Some(Cap::CmdStatus));
        assert!(user.has(Cap::CmdStatus) && !user.has(Cap::CmdSession));
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&user),
        };
        let report = dispatch(&mut ctx, "/model", "").expect("report");
        assert!(
            report.text.contains("model:"),
            "a user reads it: {}",
            report.text
        );
        let refused = dispatch(&mut ctx, "/model", "test-model").expect("refusal");
        assert!(
            refused.text.contains("cmd-session") && !refused.text.contains("test-model"),
            "a user may not switch: {}",
            refused.text
        );
        let after = dispatch(&mut ctx, "/model", "").expect("re-report");
        assert!(
            !after.text.contains("test-model"),
            "nothing changed: {}",
            after.text
        );
        let mut granted = user.clone();
        granted.caps.insert(Cap::CmdSession);
        ctx.principal = Some(&granted);
        let set = dispatch(&mut ctx, "/model", "test-model").expect("set");
        assert!(
            set.text.contains("test-model"),
            "with the grant it switches: {}",
            set.text
        );
    }

    /// A config of two plans with lists of their own, and a router that
    /// discovers. Each test names its router uniquely: the discovery
    /// cache is process-wide.
    fn models_cfg(router: &str) -> veles_core::config::VelesConfig {
        use veles_core::config::{ModelsSpec, ProviderEntry, VelesConfig};
        let list = |ids: &[&str]| ModelsSpec::List(ids.iter().map(|s| s.to_string()).collect());
        VelesConfig {
            custom_providers: vec![
                ProviderEntry {
                    name: "plan-a".into(),
                    models: list(&["m1", "m2"]),
                    ..Default::default()
                },
                ProviderEntry {
                    name: "plan-b".into(),
                    models: list(&["x1"]),
                    ..Default::default()
                },
                ProviderEntry {
                    name: router.into(),
                    // Never dialled: the catalog is seeded into the cache.
                    base_url: "http://127.0.0.1:0".into(),
                    models: list(&["fav-1", "fav-2"]),
                    discover: Some(true),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    /// Two hundred live models, one of them also curated (`fav-1`).
    fn seed_router(router: &str) {
        let mut cat: Vec<veles_core::discovery::DiscoveredModel> = (0..199)
            .map(|i| veles_core::discovery::DiscoveredModel {
                id: format!("vendor/model-{i:03}"),
                context_length: None,
            })
            .collect();
        cat.push(veles_core::discovery::DiscoveredModel {
            id: "fav-1".into(),
            context_length: None,
        });
        veles_core::discovery::put_cached(router, cat);
    }

    /// USER 2026-10-02: "use current provider if no provider
    /// specified". The session's provider, its configured models, the
    /// session's own model marked, the others named.
    #[tokio::test]
    async fn models_lists_the_sessions_provider_by_default() {
        let cfg = models_cfg("router-default");
        let out = models_command(&cfg, ("plan-a", "m2"), "").await;
        assert!(
            out.contains("plan-a (this session's provider): 2 configured"),
            "{out}"
        );
        assert!(out.contains("m1, m2 (this session)"), "{out}");
        assert!(out.contains("others: plan-b, router-default"), "{out}");
        assert!(
            !out.contains("x1"),
            "another provider's models stay out: {out}"
        );

        let named = models_command(&cfg, ("plan-a", "m2"), "PLAN-B").await;
        assert!(named.contains("plan-b: 1 configured\nx1\n"), "{named}");
        assert!(!named.contains("(this session"), "{named}");
    }

    /// USER 2026-10-02: "do not list 200 openrouter models". The
    /// configured ones are listed; the catalog's extras are counted, and
    /// searching them is capped.
    #[tokio::test]
    async fn a_routers_catalog_is_counted_and_searched_not_dumped() {
        let cfg = models_cfg("router-big");
        seed_router("router-big");
        let out = models_command(&cfg, ("plan-a", "m1"), "router-big").await;
        assert!(
            out.contains("router-big: 2 configured\nfav-1, fav-2\n"),
            "{out}"
        );
        assert!(
            out.contains("+199 more — /models router-big <text> searches them"),
            "{out}"
        );
        assert!(!out.contains("vendor/model-"), "no catalog dump: {out}");

        let narrow = models_command(&cfg, ("plan-a", "m1"), "router-big model-15").await;
        assert!(
            narrow.contains("router-big: 10 matching \"model-15\""),
            "{narrow}"
        );
        assert!(narrow.contains("vendor/model-150") && narrow.contains("vendor/model-159"));

        let wide = models_command(&cfg, ("plan-a", "m1"), "router-big vendor").await;
        assert!(wide.contains("199 matching \"vendor\""), "{wide}");
        assert_eq!(
            wide.matches("vendor/model-").count(),
            MODELS_SHOWN,
            "capped: {wide}"
        );
        assert!(wide.contains("+159 more — narrow the text"), "{wide}");
    }

    /// A first word that names no provider searches the session's; a
    /// search that finds nothing names the providers, so a misspelt one
    /// still leads somewhere.
    #[tokio::test]
    async fn an_unknown_word_is_a_search_and_a_miss_names_the_providers() {
        let cfg = models_cfg("router-miss");
        let hit = models_command(&cfg, ("plan-a", "m1"), "m2").await;
        assert!(
            hit.contains("plan-a (this session's provider): 1 matching"),
            "{hit}"
        );
        let miss = models_command(&cfg, ("plan-a", "m1"), "openroutr").await;
        assert!(miss.contains("nothing matching \"openroutr\""), "{miss}");
        assert!(
            miss.contains("providers: plan-a, plan-b, router-miss"),
            "{miss}"
        );
    }

    /// Through the shared dispatcher's blocking arm (the TUI's and the
    /// REPL's path), for a user: an answer, and the model unchanged.
    #[test]
    fn models_through_the_dispatcher_lists_and_switches_nothing() {
        use veles_core::acl::{Principal, Role};
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent_with("http://127.0.0.1:0", "models-cmd", |c| {
            c.custom_providers[0].models =
                veles_core::config::ModelsSpec::List(vec!["mock-model".into(), "other".into()]);
        });
        let user = Principal::known("irc:friend", Role::User);
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: Some(&user),
        };
        let out = dispatch(&mut ctx, "/models", "other").expect("an answer");
        assert!(
            out.text
                .contains("mock (this session's provider): 1 matching"),
            "{}",
            out.text
        );
        assert_eq!(agent.effective_model().1, "mock-model", "nothing switched");
    }

    /// `/hooks` renders the taxonomy — every KNOWN hook once, marked
    /// `fires` exactly when the engine emits it, `reserved` otherwise.
    /// The desktop shows this as a pane; here is the same report for
    /// every text surface, one implementation.
    #[test]
    fn hooks_report_classifies_every_known_hook() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "hooks");
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Tui,
            history: None,
            turn_live: false,
            principal: None,
        };
        let out = dispatch(&mut ctx, "/hooks", "").expect("hooks report");
        for h in veles_core::hooks::KNOWN_HOOKS {
            let line = out
                .text
                .lines()
                .find(|l| l.trim_start().starts_with(h))
                .unwrap_or_else(|| panic!("`{h}` missing from: {}", out.text));
            let want = if veles_core::hooks::FIRED_HOOKS.contains(h) {
                "fires"
            } else {
                "reserved"
            };
            assert!(line.contains(want), "`{h}` must be {want}: {line}");
        }
    }

    /// `/reload` tells the truth everywhere: config, commands and
    /// skills are read from disk per use, so the honest answer is
    /// "nothing to reload" — not a silent no-op, not a pretend scan.
    #[test]
    fn reload_says_nothing_needs_it() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "reload");
        let mut ctx = CommandCtx {
            agent: &agent,
            surface: Surface::Gateway,
            history: None,
            turn_live: false,
            principal: None,
        };
        let out = dispatch(&mut ctx, "/reload", "").expect("reload answers");
        assert!(
            out.text.contains("nothing to reload"),
            "the honest message: {}",
            out.text
        );
    }

    /// `/plugins` is a status command (`cmd-status`), but restarting a
    /// platform drops every conversation on it: the verbs are admin.
    #[test]
    fn plugins_reload_is_an_admin_act() {
        use veles_core::acl::{Cap, Principal, Role};
        let guest = Principal::guest();
        let user = Principal::known("echo:someone", Role::User);
        for who in [&guest, &user] {
            for verb in ["reload", "stop", "start", "unload"] {
                let out = plugins_command(Some(who), &format!("{verb} echo"))
                    .expect("a verb is answered");
                assert!(
                    out.contains(Cap::CmdAdmin.as_str()) && out.contains("denied"),
                    "{verb} by {}: {out}",
                    who.label()
                );
            }
        }
        // Allowed, but no host in this process: say where the host is.
        let out = plugins_command(None, "reload echo:a").unwrap();
        assert!(out.contains("veles plugins reload echo:a"), "{out}");
        let out = plugins_command(None, "reload").unwrap();
        assert!(out.contains("usage"), "{out}");
        // The listing is the caller's, not this function's.
        assert!(plugins_command(Some(&guest), "").is_none());
        assert!(plugins_command(Some(&guest), "list").is_none());
    }

    /// Anyone with `cmd-status` sees which process plugins run and in
    /// what state; WHY one failed (paths, a peer's message) is admin.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_plugins_last_error_is_for_admins() {
        let (home, _env) = veles_core::testenv::scratch_home();
        let dir = home.path().join("pp").join("old");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("run.sh"),
            "#!/bin/sh\necho '{\"name\":\"old\",\"version\":\"0.9\",\"protocol\":99}'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        std::fs::write(
            dir.join("plugin.toml"),
            "name = \"old\"\nkind = \"process\"\nentry = \"run.sh\"\n",
        )
        .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = veles_core::bus::GatewayBus {
            inbound: tx,
            outbound: Default::default(),
            last_chat: Default::default(),
            state_path: home.path().join("gateway-state.json"),
        };
        let host = veles_core::pluginhost::PluginHost::new(bus, "n", Default::default());
        host.start(
            &[home.path().join("pp")],
            &[],
            &[],
            serde_json::json!({}),
            std::sync::Arc::new(veles_core::pluginhost::Ungated),
        )
        .await;
        let admin = process_plugins_listing(&host, true);
        assert!(admin.contains("protocol 99"), "{admin}");
        let other = process_plugins_listing(&host, false);
        assert!(
            other.contains("old") && other.contains("refused"),
            "{other}"
        );
        assert!(!other.contains("protocol 99"), "{other}");
    }
}

#[cfg(test)]
mod acl_listing_tests {
    use super::{
        acl_command, acl_promote, defaults_command, file_rules, grant_line, grant_standing, AclCtx,
        Standing,
    };
    use std::sync::{Arc, Mutex};
    use veles_core::acl::{AclConfig, AclEntry, AclUser};
    use veles_core::pairing::Grant;

    fn grant(identity: &str, role: &str) -> Grant {
        Grant {
            identity: identity.to_string(),
            role: role.to_string(),
            expires_at: 0,
            granted_at: 1_700_000_000,
            network: None,
            channel: None,
        }
    }

    /// The verdict a listing would print a hundred seconds after these
    /// grants were written: far enough on that a past expiry is expired and
    /// a future one is not.
    fn standing(g: &Grant, cfg: &AclConfig) -> Standing {
        grant_standing(g, cfg, 1_700_000_100)
    }

    fn inert(g: &Grant, cfg: &AclConfig) -> bool {
        matches!(standing(g, cfg), Standing::Inert(_))
    }

    fn entry(identity: &str, role: &str) -> AclEntry {
        AclEntry {
            identity: identity.to_string(),
            role: role.to_string(),
            ..Default::default()
        }
    }

    /// Two lists an operator has to reconcile in their head is how a grant
    /// survives that everybody believes was revoked — but only if the split
    /// is about who DECIDES. Identity-level was not: it called a grant inert
    /// when the file merely mentioned it.
    #[test]
    fn a_grant_the_file_already_decides_is_inert() {
        let cfg = AclConfig {
            entries: vec![entry("a!*@*", "user")],
            ..Default::default()
        };
        assert!(
            inert(&grant("a!*@*", "user"), &cfg),
            "same identity, same scope, same role: the file's entry outranks the store"
        );
        match standing(&grant("zed!*@*", "trusted"), &cfg) {
            Standing::Decides(why) => assert!(
                why.contains("no rule in the file can meet"),
                "this one exists only in the store, and saying so is the point: {why}"
            ),
            Standing::Inert(why) => panic!("zed is store-only: {why}"),
        }
    }

    /// The lie this replaces. `precedence_key` compares scope BEFORE the
    /// source rank, so a file line that mentions an identity still loses to
    /// a store grant written with a scope — inside that scope. A listing
    /// that called this a redundant copy was telling the operator it was
    /// safe to delete the rule that was actually deciding.
    #[test]
    fn a_scoped_grant_is_not_covered_by_the_files_unscoped_line() {
        let cfg = AclConfig {
            entries: vec![entry("octo!*@*", "user")],
            ..Default::default()
        };
        let g = Grant {
            network: Some("libera".to_string()),
            channel: Some("#veles".to_string()),
            ..grant("octo!*@*", "user")
        };
        match standing(&g, &cfg) {
            Standing::Decides(why) => {
                assert!(
                    why.contains("scope"),
                    "the reason has to name the comparison that decides it: {why}"
                );
                assert!(why.contains("in #veles"), "{why}");
            }
            Standing::Inert(why) => {
                panic!("the grant wins inside #veles (see acl::tests::a_scoped_grant_beats_the_files_unscoped_line): {why}")
            }
        }
        // And the mirror image: a file line narrowed to one network does not
        // reach the others this grant does.
        let cfg = AclConfig {
            entries: vec![AclEntry {
                network: Some("libera".to_string()),
                ..entry("octo!*@*", "user")
            }],
            ..Default::default()
        };
        assert!(
            !inert(&grant("octo!*@*", "user"), &cfg),
            "the grant decides on every network the file's line does not name"
        );
    }

    /// A lower role wins a tie whichever source it came from, so a runtime
    /// downgrade of a file-declared owner is the emergency brake
    /// `pairing::grant` documents — live, not a duplicate.
    #[test]
    fn a_store_downgrade_beats_the_file_that_names_the_identity() {
        let cfg = AclConfig {
            owner: vec!["irc:boss".to_string()],
            ..Default::default()
        };
        match standing(&grant("irc:boss", "user"), &cfg) {
            Standing::Decides(why) => assert!(why.contains("lower role"), "{why}"),
            Standing::Inert(why) => {
                panic!("deleting this copy would hand the identity owner back: {why}")
            }
        }
        // The same identity with a HIGHER store role loses, which is the
        // case where the copy really is dead weight.
        let cfg = AclConfig {
            roles: [("irc:pal".to_string(), "user".to_string())].into(),
            ..Default::default()
        };
        assert!(
            inert(&grant("irc:pal", "trusted"), &cfg),
            "the file's lower role wins, so the store copy decides nothing"
        );
    }

    /// The file declares identities in four shapes and all four become rules
    /// resolution compares, so all four are candidates here. Missing one
    /// would report a duplicate as store-only, and the operator would "fix"
    /// it by writing it twice.
    #[test]
    fn every_shape_the_file_declares_is_a_candidate() {
        let cfg = AclConfig {
            owner: vec!["irc:xenia".to_string()],
            roles: [("irc:friend".to_string(), "trusted".to_string())].into(),
            entries: vec![entry("a!*@*", "user")],
            users: vec![AclUser {
                name: "ops".to_string(),
                role: "admin".to_string(),
                identities: vec!["telegram:12345".to_string()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rules = file_rules(&cfg);
        let mut ids: Vec<&str> = rules.iter().map(|r| r.identity).collect();
        ids.sort();
        assert_eq!(
            ids,
            vec!["a!*@*", "irc:friend", "irc:xenia", "telegram:12345"],
            "one of the four shapes is not being counted"
        );
        for want in ["owner list", "role pin", "entry", "user `ops`"] {
            assert!(
                rules.iter().any(|r| r.source == want),
                "the sentence that explains a verdict names the shape: {want}"
            );
        }
        // A named user's rule outranks the store at an equal role.
        assert!(
            inert(&grant("telegram:12345", "admin"), &cfg),
            "the user shape can make a grant inert too"
        );
    }

    /// A grant carries no platform, so a file line narrowed to one matches
    /// strictly fewer callers and cannot cover it.
    #[test]
    fn a_platform_narrowed_line_does_not_cover_a_grant() {
        let cfg = AclConfig {
            entries: vec![AclEntry {
                platform: Some("irc".to_string()),
                ..entry("irc:pal", "user")
            }],
            ..Default::default()
        };
        assert!(!inert(&grant("irc:pal", "user"), &cfg));
    }

    /// Resolution skips an expired grant and one whose role does not parse,
    /// so neither copy decides anything — but the listing has to say WHICH
    /// of those it is, or "inert" reads as "the file covers this".
    #[test]
    fn a_grant_resolution_skips_decides_nothing() {
        let dead = Grant {
            expires_at: 1_700_000_050,
            ..grant("old!*@*", "trusted")
        };
        match standing(&dead, &AclConfig::default()) {
            Standing::Inert(why) => assert!(why.contains("expired"), "{why}"),
            Standing::Decides(why) => panic!("resolution skips it: {why}"),
        }
        let live = Grant {
            expires_at: 1_800_000_000,
            ..grant("new!*@*", "trusted")
        };
        assert!(
            !inert(&live, &AclConfig::default()),
            "an expiry in the future is a grant that still decides"
        );
        match standing(&grant("x!*@*", "emperor"), &AclConfig::default()) {
            Standing::Inert(why) => assert!(why.contains("does not parse"), "{why}"),
            Standing::Decides(why) => panic!("an unparseable role is never applied: {why}"),
        }
    }

    /// A grant narrowed to a channel is not the rule an unscoped line
    /// declares, and one with an expiry is not a permanent rule — both have
    /// to be visible next to the verdict rather than folded into an identity
    /// comparison nobody can check.
    #[test]
    fn the_scope_expiry_and_reason_are_shown() {
        let g = Grant {
            network: Some("libera".to_string()),
            channel: Some("#veles".to_string()),
            expires_at: 1_800_000_000,
            ..grant("octo!*@*", "user")
        };
        let line = grant_line(&g, &AclConfig::default(), 1_700_000_100);
        assert!(
            line.contains("on libera in #veles"),
            "the scope is the difference between two rules: {line}"
        );
        assert!(line.contains("expires 1800000000"), "{line}");
        assert!(
            line.contains("no rule in the file can meet"),
            "the verdict belongs beside the grant: {line}"
        );
        let plain = grant_line(
            &grant("a!*@*", "user"),
            &AclConfig::default(),
            1_700_000_100,
        );
        assert!(
            plain.contains("  `a!*@*` = user (granted 1700000000)"),
            "an unscoped, non-expiring grant reads without either: {plain}"
        );
        let dead = grant_line(
            &Grant {
                expires_at: 1_700_000_050,
                ..grant("old!*@*", "user")
            },
            &AclConfig::default(),
            1_700_000_100,
        );
        assert!(
            dead.contains("EXPIRED"),
            "a past expiry must not read as a live grant: {dead}"
        );
    }

    // ── /acl promote ──────────────────────────────────────────────────

    /// One grant to put in the store before promoting. A struct rather
    /// than a five-tuple because a tuple of options is unreadable at the
    /// call site, which is the only place a test's claim is legible.
    struct Seed {
        id: &'static str,
        role: &'static str,
        ttl: Option<u64>,
        net: Option<&'static str>,
        chan: Option<&'static str>,
    }

    impl Seed {
        fn new(id: &'static str, role: &'static str) -> Self {
            Seed {
                id,
                role,
                ttl: None,
                net: None,
                chan: None,
            }
        }
        fn scoped(mut self, net: Option<&'static str>, chan: Option<&'static str>) -> Self {
            self.net = net;
            self.chan = chan;
            self
        }
        fn ttl(mut self, secs: u64) -> Self {
            self.ttl = Some(secs);
            self
        }
    }

    /// A runtime user to seed the store with.
    struct UserSeed {
        name: &'static str,
        role: &'static str,
        ids: &'static [&'static str],
    }

    /// A store in the scratch home, seeded with both shapes the runtime
    /// half of the ACL can hold.
    fn seeded_store(grants: &[Seed], users: &[UserSeed]) -> Arc<Mutex<veles_core::store::Store>> {
        let store = Arc::new(Mutex::new(
            veles_core::store::Store::open(&veles_core::config::veles_home().join("state.db"))
                .unwrap(),
        ));
        for g in grants {
            veles_core::pairing::grant_scoped(
                &store,
                g.id,
                g.role,
                g.ttl,
                g.net.map(str::to_string),
                g.chan.map(str::to_string),
            )
            .unwrap();
        }
        for u in users {
            let ids: Vec<String> = u.ids.iter().map(|s| s.to_string()).collect();
            veles_core::pairing::user_save(&store, u.name, u.role, &ids).unwrap();
        }
        store
    }

    /// A scratch home, a store holding `grants` and `users`, and what
    /// promote did: the report, config.toml afterwards, and what the store
    /// is still holding.
    fn promote_users(
        cfg_toml: Option<&str>,
        grants: &[Seed],
        users: &[UserSeed],
        flags: &[&str],
    ) -> (String, String, Vec<Grant>, Vec<AclUser>) {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        if let Some(text) = cfg_toml {
            std::fs::write(&path, text).unwrap();
        }
        veles_core::acl::invalidate_live_config();
        let store = seeded_store(grants, users);
        let fallback = AclConfig::default();
        let ctx = AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: None,
            sender: None,
        };
        let out = acl_promote(&ctx, flags);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let left = veles_core::pairing::list_grants(&store);
        let users_left = veles_core::pairing::list_users(&store);
        (out, text, left, users_left)
    }

    // ── /defaults ────────────────────────────────────────────────────

    fn defaults_ctx_run(cfg_toml: &str, rest: &str) -> (String, String) {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, cfg_toml).unwrap();
        veles_core::acl::invalidate_live_config();
        let store = seeded_store(&[], &[]);
        let fallback = AclConfig::default();
        let ctx = AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: None,
            sender: None,
        };
        let out = defaults_command(&ctx, rest);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        (out, text)
    }

    const DEFAULTS_CFG: &str = r#"
[model]
provider = "ali-team"
default = "flash"

[imagegen]
provider = "ali-team"
model = "qwen-image-3.0-pro"

[[custom_providers]]
name = "ali-team"
base_url = "http://127.0.0.1:0/v1"

[[custom_providers]]
name = "ali-personal"
base_url = "http://127.0.0.1:2/v1"
"#;

    /// The five tables that answer one question get printed together,
    /// because "what am I spending on" is one question and the config
    /// spreads it over `[model]`, `[vision]`, `[imagegen]`, `[videogen]`,
    /// `[asr]` and four loose `agent.tts_*` fields.
    #[test]
    fn defaults_with_no_argument_reads_every_route_in_one_block() {
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "");
        for label in ["chat", "vision", "image", "video", "asr", "tts"] {
            assert!(out.contains(label), "{label} missing from: {out}");
        }
        assert!(out.contains("ali-team"), "{out}");
        assert!(out.contains("qwen-image-3.0-pro"), "{out}");
        assert!(out.contains("(unset)"), "an unset route says so: {out}");
        assert!(
            out.contains("/model changes THIS session only"),
            "the listing must draw the distinction it exists for: {out}"
        );
    }

    /// The 2026-10-03 change in one command: every route on one plan
    /// moved to the other — previewed, then written — and nothing else.
    #[test]
    fn defaults_switch_moves_every_route_on_a_provider_in_one_write() {
        let cfg = format!(
            "{DEFAULTS_CFG}\n[[command_aliases]]\nname = \"img\"\nkind = \"image\"\n\
             provider = \"ali-team\"\nmodel = \"qwen-image-3.0-pro\"\n\n\
             [[command_aliases]]\nname = \"other\"\nprovider = \"elsewhere\"\nmodel = \"m\"\n\n\
             [model_aliases.team]\nprovider = \"ali-team\"\nmodel = \"flash\"\n"
        );
        let (preview, text) = defaults_ctx_run(&cfg, "switch ali-team ali-personal");
        assert!(preview.contains("3 route(s) on ali-team"), "{preview}");
        assert!(preview.contains("model.provider"), "{preview}");
        assert!(preview.contains("imagegen.provider"), "{preview}");
        assert!(preview.contains("[[command_aliases]] img"), "{preview}");
        assert_eq!(text, cfg, "a preview writes nothing");

        let (out, text) = defaults_ctx_run(&cfg, "switch ali-team ali-personal apply");
        assert!(out.contains("new sessions"), "{out}");
        let parsed: veles_core::config::VelesConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.model.provider.as_deref(), Some("ali-personal"));
        assert_eq!(parsed.imagegen.provider.as_deref(), Some("ali-personal"));
        assert_eq!(parsed.command_aliases[0].provider, "ali-personal");
        assert_eq!(
            parsed.command_aliases[1].provider, "elsewhere",
            "not on ali-team"
        );
        assert_eq!(
            parsed.model_aliases["team"].provider, "ali-team",
            "an alias names one route on purpose and stays"
        );
        assert_eq!(
            parsed.model.default.as_deref(),
            Some("flash"),
            "models stay"
        );
    }

    /// A model the target does not list is flagged, not silently moved.
    #[test]
    fn defaults_switch_flags_a_model_the_target_does_not_serve() {
        let cfg = DEFAULTS_CFG.replace(
            "name = \"ali-personal\"\nbase_url = \"http://127.0.0.1:2/v1\"",
            "name = \"ali-personal\"\nbase_url = \"http://127.0.0.1:2/v1\"\nmodels = [\"pro\"]",
        );
        let (out, _) = defaults_ctx_run(&cfg, "switch ali-team ali-personal");
        assert!(
            out.contains("flash is not in ali-personal's model list"),
            "{out}"
        );
        // A provider's `models` lists chat models: an image model is
        // never in it, so an image route is not flagged for that.
        assert!(!out.contains("qwen-image-3.0-pro is not"), "{out}");
    }

    /// The flip the user actually needs — ali-personal is out of quota,
    /// put everything on ali-team — and the honest report that it does
    /// NOT affect the session you typed it in.
    #[test]
    fn defaults_provider_writes_the_file_and_says_what_it_affects() {
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "provider ali-personal");
        assert!(out.contains("model.provider = ali-personal"), "{out}");
        assert!(
            out.contains("new sessions"),
            "`model.*` is NewSessions — saying plain \"ok\" is how someone \
             concludes the knob is broken: {out}"
        );
        assert!(
            text.contains("provider = \"ali-personal\""),
            "it must reach the file: {text}"
        );
        assert!(
            text.contains("default = \"flash\""),
            "and leave the rest alone: {text}"
        );
    }

    /// An unconfigured provider is refused HERE, naming what is
    /// configured. Writing it would not fail until some later session's
    /// first turn, by which point nothing points back at this command.
    #[test]
    fn defaults_refuses_a_provider_that_is_not_configured() {
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "provider nosuch");
        assert!(out.contains("not configured"), "{out}");
        assert!(
            out.contains("ali-team"),
            "it must name the real ones: {out}"
        );
        assert!(out.contains("ali-personal"), "{out}");
        assert!(
            !text.contains("nosuch"),
            "and nothing may reach the file: {text}"
        );
    }

    /// The media routes take a provider and an optional model, and TTS
    /// lands on `agent.tts_*` rather than a section of its own — an
    /// asymmetry that is in the config, not invented by the command.
    #[test]
    fn defaults_writes_the_media_routes_including_the_odd_one() {
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "image ali-personal wan2.7-image");
        assert!(out.contains("imagegen.provider = ali-personal"), "{out}");
        assert!(out.contains("imagegen.model = wan2.7-image"), "{out}");
        assert!(text.contains("wan2.7-image"), "{text}");

        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "tts ali-personal some-voice-model");
        assert!(out.contains("agent.tts_provider = ali-personal"), "{out}");
        assert!(
            text.contains("tts_provider = \"ali-personal\""),
            "TTS is four loose [agent] fields, not a GenSection: {text}"
        );
        assert!(text.contains("tts_model = \"some-voice-model\""), "{text}");
    }

    /// `/defaults model` RESOLVES the spelling and writes the two keys
    /// `[model]` actually reads.
    ///
    /// It used to store the argument verbatim, on the reasoning that
    /// `/model`'s resolver would pick it up later. Nothing ever resolves
    /// `[model] default`: `build_routes` reads it as a bare model id
    /// against `[model] provider`, so `/defaults model
    /// ali-personal/flash` sent that whole composite as a model id to
    /// `ali-team` — an instant 404, from a command whose own usage line
    /// advertised the spelling. Observed live 2026-09-12.
    #[test]
    fn defaults_model_resolves_the_spelling_into_the_two_keys_it_writes() {
        // `model@provider` — the unambiguous spelling.
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "model flash@ali-personal");
        assert!(
            text.contains("provider = \"ali-personal\""),
            "the provider half must move too, or the model is asked for \
             from the wrong provider: {text}"
        );
        assert!(
            text.contains("default = \"flash\""),
            "and the model half must be the MODEL, not the composite: {text}"
        );
        assert!(
            !text.contains("flash@ali-personal"),
            "the composite is not a model id and must not survive: {text}"
        );
        assert!(
            out.contains("ali-personal/flash"),
            "says what it resolved to: {out}"
        );

        // `provider/model` — the spelling that was advertised and broken.
        let (_, text) = defaults_ctx_run(DEFAULTS_CFG, "model ali-personal/flash");
        assert!(text.contains("provider = \"ali-personal\""), "{text}");
        assert!(text.contains("default = \"flash\""), "{text}");
    }

    /// A slashed MODEL ID is not a route. `openrouter/free` is a real
    /// model id on this box, and step 3 claims it before step 3.5 can
    /// split it — the ordering `resolve_route` exists to preserve.
    #[test]
    fn defaults_model_keeps_a_slashed_model_id_whole() {
        let cfg = format!(
            "{DEFAULTS_CFG}\n[[custom_providers]]\nname = \"or\"\nbase_url = \"http://127.0.0.1:3/v1\"\n\
             models = [\"openrouter/free\"]\n"
        );
        let (_, text) = defaults_ctx_run(&cfg, "model openrouter/free");
        assert!(
            text.contains("default = \"openrouter/free\""),
            "the whole string is the model id: {text}"
        );
        assert!(
            text.contains("provider = \"or\""),
            "and it belongs to the provider that lists it: {text}"
        );
    }

    /// An alias resolves to its route, because `[model] default` is read
    /// raw and an alias NAME stored there would be sent to the provider
    /// as a model id.
    #[test]
    fn defaults_model_expands_an_alias_rather_than_storing_its_name() {
        let cfg = format!(
            "{DEFAULTS_CFG}\n[model_aliases]\nteam = {{ provider = \"ali-team\", model = \"flash\" }}\n"
        );
        let (_, text) = defaults_ctx_run(&cfg, "model team");
        assert!(text.contains("default = \"flash\""), "{text}");
        assert!(text.contains("provider = \"ali-team\""), "{text}");
        assert!(!text.contains("default = \"team\""), "{text}");
    }

    /// A bare model id nobody lists is still accepted — a provider may
    /// not enumerate its models — but the provider is NOT moved and the
    /// reply carries the resolver's own UNVERIFIED warning.
    #[test]
    fn defaults_model_leaves_the_provider_alone_for_an_unverified_bare_name() {
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "model some-unlisted-model");
        assert!(text.contains("default = \"some-unlisted-model\""), "{text}");
        assert!(
            text.contains("provider = \"ali-team\""),
            "the provider is a guess-free part of the config and must not move: {text}"
        );
        assert!(out.contains("UNVERIFIED"), "the guess is visible: {out}");
    }

    /// A spelling that names no known provider is refused HERE, naming
    /// what is configured — the same rule `/defaults provider` follows.
    /// Writing it would not fail until some later session's first turn.
    #[test]
    fn defaults_model_refuses_a_route_it_cannot_resolve() {
        let (out, text) = defaults_ctx_run(DEFAULTS_CFG, "model flash@nosuch");
        assert!(out.contains("nosuch"), "names the unknown provider: {out}");
        assert!(
            !text.contains("nosuch"),
            "and nothing may reach the file: {text}"
        );
        assert!(
            text.contains("default = \"flash\""),
            "the old route survives: {text}"
        );
    }

    /// `/defaults bind` → `binds` → `unbind`, round trip, and the two
    /// refusals that keep a dead rule out of the store.
    #[test]
    fn defaults_bind_round_trips_and_refuses_a_rule_that_could_not_fire() {
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "binds");
        assert!(out.contains("no model binds"), "{out}");
        assert!(out.contains("/defaults bind"), "and says how: {out}");

        // A rule with no route would match and change nothing.
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "bind platform=irc");
        assert!(out.contains("needs a route"), "{out}");

        // The role spelling that used to be a silent no-op is now a
        // refusal at the moment you type it.
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "bind role=<=nonsense model=m");
        assert!(out.contains("is not a role"), "{out}");

        // …and an unconfigured provider, named, with the real ones.
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "bind provider=nosuch");
        assert!(out.contains("not configured"), "{out}");
        assert!(out.contains("ali-team"), "{out}");

        // Unknown selectors are reported rather than silently dropped —
        // a typo'd `chanel=` would otherwise widen the rule to every
        // channel without saying so.
        let (out, _) = defaults_ctx_run(DEFAULTS_CFG, "bind chanel=#x model=m");
        assert!(out.contains("not a selector"), "{out}");
        assert!(out.contains("chanel=#x"), "{out}");
    }

    /// The store half, driven through ONE ctx so the rows persist across
    /// the three calls — `defaults_ctx_run` opens a fresh store per call,
    /// which is right for the file-writing arms and useless here.
    #[test]
    fn a_runtime_bind_is_stored_listed_and_removed() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, DEFAULTS_CFG).unwrap();
        veles_core::acl::invalidate_live_config();
        let store = seeded_store(&[], &[]);
        let fallback = AclConfig::default();
        let ctx = AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: None,
            sender: None,
        };

        let out = defaults_command(
            &ctx,
            "bind platform=irc role=<=trusted model=cheap provider=ali-team",
        );
        assert!(out.contains("bound"), "{out}");
        assert!(
            out.contains("cheap@ali-team"),
            "the route, unambiguously: {out}"
        );

        let out = defaults_command(&ctx, "binds");
        assert!(out.contains("role=<=trusted"), "{out}");
        assert!(out.contains("platform=irc"), "{out}");

        // Same selectors → REPLACE, not a second invisible rule.
        defaults_command(
            &ctx,
            "bind platform=irc role=<=trusted model=other provider=ali-team",
        );
        let out = defaults_command(&ctx, "binds");
        assert_eq!(
            out.matches("role=<=trusted").count(),
            1,
            "re-binding the same selectors must replace: {out}"
        );
        assert!(out.contains("other@ali-team"), "{out}");

        let out = defaults_command(&ctx, "unbind platform=irc role=<=trusted");
        assert!(out.contains("unbound"), "{out}");
        assert!(defaults_command(&ctx, "binds").contains("no model binds"));
        assert!(
            defaults_command(&ctx, "unbind platform=irc role=<=trusted")
                .contains("no runtime bind"),
            "a second unbind says so rather than claiming success"
        );
    }

    fn promote(
        cfg_toml: Option<&str>,
        grants: &[Seed],
        flags: &[&str],
    ) -> (String, String, Vec<Grant>) {
        let (out, text, left, _) = promote_users(cfg_toml, grants, &[], flags);
        (out, text, left)
    }

    /// One `/acl …` line against a scratch home, with the store seeded and
    /// the file read back afterwards: a command that says it wrote
    /// something has to be checked against the thing it claims to have
    /// written. `who` resolves a caller instead of using the owner surface,
    /// which is how the tier checks get exercised.
    fn acl_run(
        cfg_toml: Option<&str>,
        grants: &[Seed],
        users: &[UserSeed],
        line: &str,
        who: Option<&str>,
    ) -> (String, String) {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let path = veles_core::configedit::config_path();
        if let Some(text) = cfg_toml {
            std::fs::write(&path, text).unwrap();
        }
        veles_core::acl::invalidate_live_config();
        let store = seeded_store(grants, users);
        let fallback = AclConfig::default();
        let live = veles_core::acl::live_config();
        let principal = who.map(|id| {
            veles_core::acl::Principal::resolve_ctx(
                Some(id),
                &veles_core::acl::SenderContext::default(),
                &live,
            )
        });
        let ctx = AclCtx {
            store: &store,
            fallback: &fallback,
            session_id: "",
            principal: principal.as_ref(),
            sender: None,
        };
        let out = acl_command(&ctx, line);
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        (out, text)
    }

    const CONFIGURED: &str = "# the operator's own note\n[acl]\nowner = [\"irc:me\"]\n\n[agent]\n";

    /// The same file with one narrow rule in it: the shape that makes a
    /// store grant partly covered rather than duplicated or unrelated.
    const NARROW: &str = "# the operator's own note\n[acl]\nowner = [\"irc:me\"]\n\n\
                          [[acl.entries]]\nidentity = \"octo!*@gateway/veles\"\nrole = \
                          \"banned\"\n\n[agent]\n";

    /// The whole point: a grant the store decides becomes a line in the
    /// file, the store copy goes away, and nothing else in the file moves.
    #[test]
    fn promote_writes_the_grant_the_store_decides_and_drops_the_copy() {
        let (out, text, left) = promote(
            Some(CONFIGURED),
            &[Seed::new("bob!*@*", "trusted").scoped(Some("libera"), None)],
            &[],
        );
        assert!(out.contains("promoted 1 runtime grant into"), "{out}");
        assert!(
            out.contains("in effect now"),
            "[acl] is re-read per use, so the report must not ask for a restart: {out}"
        );
        assert!(text.contains("[[acl.entries]]"), "{text}");
        assert!(text.contains("identity = \"bob!*@*\""), "{text}");
        assert!(text.contains("role = \"trusted\""), "{text}");
        assert!(text.contains("network = \"libera\""), "{text}");
        assert!(
            text.contains("# was a runtime grant"),
            "the file should say where the rule came from: {text}"
        );
        assert!(
            text.contains("# the operator's own note") && text.contains("[agent]"),
            "the append must not disturb the rest of the file: {text}"
        );
        assert!(
            left.is_empty(),
            "the store copy is dropped so the file is the one source: {left:?}"
        );
        // And what was written still reads back as the rule it came from.
        let cfg: veles_core::config::VelesConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.acl.entries.len(), 1);
        assert_eq!(cfg.acl.entries[0].identity, "bob!*@*");
        assert_eq!(cfg.acl.entries[0].network.as_deref(), Some("libera"));
        assert_eq!(cfg.acl.owner, vec!["irc:me".to_string()]);
    }

    #[test]
    fn a_dry_run_writes_nothing_and_shows_the_exact_lines() {
        let (out, text, left) = promote(
            Some(CONFIGURED),
            &[Seed::new("bob!*@*", "trusted")],
            &["--dry-run"],
        );
        assert!(out.contains("dry run"), "{out}");
        assert!(
            out.contains("[[acl.entries]]") && out.contains("identity = \"bob!*@*\""),
            "the operator has to see what a real run would write: {out}"
        );
        assert_eq!(text, CONFIGURED, "a dry run that writes is not a dry run");
        assert_eq!(left.len(), 1, "and it must not drop the store copy either");
    }

    /// Writing the first `[[acl.entries]]` line into a file with no `[acl]`
    /// switches the whole ACL from dormant to curated, and every identity
    /// the file does not name — the operator's included — falls to `guest`.
    /// A maintenance command cannot be the thing that does that by accident.
    #[test]
    fn promote_refuses_to_be_the_thing_that_locks_the_operator_out() {
        let (out, text, left) = promote(None, &[Seed::new("bob!*@*", "trusted")], &[]);
        assert!(out.contains("refusing"), "{out}");
        assert!(
            out.contains("guest") && out.contains("OWNER"),
            "both halves of the consequence have to be named: {out}"
        );
        assert!(text.is_empty(), "no file was created: {text}");
        assert_eq!(left.len(), 1, "the grant is untouched");

        // A dry run says it would refuse, and still shows the lines.
        let (out, text, _) = promote(None, &[Seed::new("bob!*@*", "trusted")], &["--dry-run"]);
        assert!(out.contains("would refuse"), "{out}");
        assert!(out.contains("[[acl.entries]]"), "{out}");
        assert!(text.is_empty(), "{text}");
    }

    /// `[[acl.entries]]` has no expiry, so promoting a temporary grant
    /// silently makes it permanent. That has to be asked for.
    #[test]
    fn a_grant_that_still_expires_is_not_made_permanent_by_default() {
        let grants = [Seed::new("bob!*@*", "trusted").ttl(3600)];
        let (out, text, left) = promote(Some(CONFIGURED), &grants, &[]);
        assert!(out.contains("left in the store"), "{out}");
        assert!(
            out.contains("cannot expire"),
            "the reason is the expiry, not an oversight: {out}"
        );
        assert!(!text.contains("[[acl.entries]]"), "{text}");
        assert_eq!(left.len(), 1);

        let (out, text, left) = promote(Some(CONFIGURED), &grants, &["--include-expiring"]);
        assert!(text.contains("[[acl.entries]]"), "{text}");
        assert!(
            out.contains("expiry") && out.contains("dropped"),
            "say the expiry went: {out}"
        );
        assert!(
            text.contains("expiry"),
            "and say it in the file, where the rule now lives: {text}"
        );
        assert!(left.is_empty());
    }

    /// A grant the file already decides is not written — a second rule for
    /// one identity is a file with two opinions — and it is not deleted
    /// either, unless the operator asks.
    #[test]
    fn a_grant_the_file_decides_is_neither_written_nor_pruned_unless_asked() {
        let toml = "[acl]\nowner = [\"irc:me\"]\n\n[[acl.entries]]\nidentity = \"bob!*@*\"\nrole = \"user\"\n";
        let grants = [Seed::new("bob!*@*", "user")];
        let (out, text, left) = promote(Some(toml), &grants, &[]);
        assert!(out.contains("nothing to write"), "{out}");
        assert_eq!(
            text.matches("[[acl.entries]]").count(),
            1,
            "no second rule for one identity: {text}"
        );
        assert_eq!(left.len(), 1, "left alone without --prune");

        let (out, _text, left) = promote(Some(toml), &grants, &["--prune"]);
        assert!(out.contains("pruned 1 grant"), "{out}");
        assert!(left.is_empty(), "the dead copy is gone");
    }

    #[test]
    fn promote_reports_an_empty_store_and_an_unknown_flag() {
        let (out, _, _) = promote(Some(CONFIGURED), &[], &[]);
        assert!(out.contains("nothing to promote"), "{out}");
        let (out, _, _) = promote(Some(CONFIGURED), &[], &["--everything"]);
        assert!(out.contains("unknown flag"), "{out}");
        assert!(
            out.contains("--include-expiring") && out.contains("--users"),
            "usage names the flags: {out}"
        );
    }

    /// The case the identity-string comparison could not see. The file bans
    /// `octo` on one host; the store grants `octo` everywhere. Neither is a
    /// duplicate of the other: the file wins on that host and the grant
    /// decides on every other one, so both halves belong in the report — and
    /// the old comparison, which asked whether any string in the file equalled
    /// the grant's, answered "no rule in the file names this identity".
    #[test]
    fn a_grant_partly_covered_by_a_narrower_file_mask_still_decides() {
        let cfg = AclConfig {
            entries: vec![entry("octo!*@gateway/veles", "banned")],
            ..Default::default()
        };
        match standing(&grant("octo!*@*", "user"), &cfg) {
            Standing::Decides(why) => {
                assert!(why.contains("narrower"), "{why}");
                assert!(why.contains("gateway/veles"), "{why}");
                assert!(
                    why.contains("loses to it inside"),
                    "and says where the file takes over: {why}"
                );
            }
            Standing::Inert(why) => {
                panic!("the grant decides for every caller off that host: {why}")
            }
        }

        // Promote writes it, and the write carries the same sentence: the
        // operator is told what the two rules now share before the file is
        // the only place either of them can be read.
        let (out, text, left) = promote(Some(NARROW), &[Seed::new("octo!*@*", "user")], &[]);
        assert!(out.contains("promoted 1 runtime grant into"), "{out}");
        assert!(
            out.contains("narrower"),
            "the report names the interaction: {out}"
        );
        assert!(text.contains("identity = \"octo!*@*\""), "{text}");
        assert_eq!(
            text.matches("[[acl.entries]]").count(),
            2,
            "the file's own line is still there, and the new one is beside it: {text}"
        );
        assert!(left.is_empty(), "{left:?}");
    }

    /// The mirror: a grant MORE specific than the file's line wins where they
    /// meet, so it is live even at the same role. Calling it dead weight
    /// because the file "already has that nick" is the same mistake from the
    /// other direction.
    #[test]
    fn a_more_specific_grant_outranks_the_files_wider_line() {
        let cfg = AclConfig {
            entries: vec![entry("octo!*@*", "user")],
            ..Default::default()
        };
        match standing(&grant("octo!*@gateway/veles", "user"), &cfg) {
            Standing::Decides(why) => {
                assert!(why.contains("outranks"), "{why}");
                assert!(
                    why.contains("more specific mask"),
                    "the reason names the comparison that decided it: {why}"
                );
            }
            Standing::Inert(why) => panic!("this grant is the rule resolution picks: {why}"),
        }
    }

    /// An exact identity and a mask meet only on a caller carrying both, so
    /// which callers those are is not knowable from two texts. The verdict
    /// says that instead of guessing — while the ORDER is still resolution's,
    /// exact identity first.
    #[test]
    fn an_exact_identity_and_a_mask_are_not_compared_by_guessing() {
        let cfg = AclConfig {
            entries: vec![entry("octo!*@*", "banned")],
            ..Default::default()
        };
        match standing(&grant("irc:octo", "user"), &cfg) {
            Standing::Decides(why) => {
                assert!(why.contains("cannot be compared"), "{why}");
                assert!(why.contains("exact identity"), "{why}");
            }
            Standing::Inert(why) => panic!("nothing proves that mask covers this account: {why}"),
        }
    }

    /// Two rules that cannot meet are not duplicates, and calling one of them
    /// dead weight is how a working rule gets pruned.
    #[test]
    fn rules_that_cannot_meet_are_not_called_duplicates() {
        let cfg = AclConfig {
            entries: vec![AclEntry {
                network: Some("oftc".to_string()),
                ..entry("octo!*@*", "user")
            }],
            ..Default::default()
        };
        let g = Grant {
            network: Some("libera".to_string()),
            ..grant("octo!*@*", "user")
        };
        match standing(&g, &cfg) {
            Standing::Decides(why) => {
                assert!(why.contains("no rule in the file can meet"), "{why}")
            }
            Standing::Inert(why) => panic!("the file's line is on another network: {why}"),
        }
    }

    /// A runtime user is a rule like any other and the file has a shape for
    /// it, so `--users` folds it in: same writer, same backup, same "in effect
    /// now", and the store copy goes away the way a promoted grant's does.
    #[test]
    fn promote_users_writes_the_runtime_user_and_drops_the_copy() {
        let users = [UserSeed {
            name: "nyx",
            role: "trusted",
            ids: &["nyx!~n@*.example.com", "telegram:77"],
        }];
        let (out, text, _grants, left) = promote_users(Some(CONFIGURED), &[], &users, &["--users"]);
        assert!(out.contains("promoted 1 runtime user into"), "{out}");
        assert!(
            out.contains("in effect now"),
            "[acl] is re-read per use, so the report must not ask for a restart: {out}"
        );
        assert!(text.contains("[[acl.users]]"), "{text}");
        assert!(text.contains("name = \"nyx\""), "{text}");
        assert!(text.contains("role = \"trusted\""), "{text}");
        assert!(
            text.contains("\"nyx!~n@*.example.com\"") && text.contains("\"telegram:77\""),
            "every identity travels, or the user arrives narrower than the store had them: {text}"
        );
        assert!(
            text.contains("# was a runtime user"),
            "the file says where the rule came from: {text}"
        );
        assert!(
            text.contains("# the operator's own note") && text.contains("[agent]"),
            "the append must not disturb the rest of the file: {text}"
        );
        assert!(
            left.is_empty(),
            "the store copy is dropped so the file is the one source: {left:?}"
        );
        let cfg: veles_core::config::VelesConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.acl.users.len(), 1);
        assert_eq!(cfg.acl.users[0].name, "nyx");
        assert_eq!(cfg.acl.users[0].identities.len(), 2);
        assert_eq!(cfg.acl.owner, vec!["irc:me".to_string()]);
    }

    /// A user the file already carries — same name, same role, identities
    /// covered — is dead weight, and `--prune` drops it. Writing a second
    /// table under the same name would leave the file with two opinions about
    /// one subject.
    #[test]
    fn a_runtime_user_the_file_already_carries_is_inert_and_prunable() {
        let toml = "[acl]\nowner = [\"irc:me\"]\n\n[[acl.users]]\nname = \"nyx\"\nrole = \
                    \"trusted\"\nidentities = [\"nyx!~n@*.example.com\"]\n";
        let users = [UserSeed {
            name: "nyx",
            role: "trusted",
            ids: &["nyx!~n@*.example.com"],
        }];
        let (out, text, _, left) = promote_users(Some(toml), &[], &users, &["--users"]);
        assert!(out.contains("already decides"), "{out}");
        assert!(
            out.contains("carries every one of these identities"),
            "{out}"
        );
        assert_eq!(
            text.matches("[[acl.users]]").count(),
            1,
            "no second table for one name: {text}"
        );
        assert_eq!(left.len(), 1, "left alone without --prune");

        let (out, _, _, left) = promote_users(Some(toml), &[], &users, &["--users", "--prune"]);
        assert!(out.contains("pruned 1 runtime user"), "{out}");
        assert!(left.is_empty(), "the dead copy is gone");
    }

    /// The refusal a grant cannot have: the file already uses the NAME and
    /// says something different. Promote will not pick between them, because
    /// two `[[acl.users]]` tables under one name leave resolution merging two
    /// roles for one subject and nobody able to say which was meant.
    #[test]
    fn a_runtime_user_the_file_contradicts_is_not_written() {
        let toml = "[acl]\nowner = [\"irc:me\"]\n\n[[acl.users]]\nname = \"nyx\"\nrole = \
                    \"user\"\nidentities = [\"nyx!~n@*.example.com\"]\n";
        let users = [UserSeed {
            name: "nyx",
            role: "trusted",
            ids: &["nyx!~n@*.example.com"],
        }];
        let (out, text, _, left) = promote_users(Some(toml), &[], &users, &["--users"]);
        assert!(out.contains("NOT written"), "{out}");
        assert!(
            out.contains("the file's user `nyx` is user and the store copy is trusted"),
            "the report says which two rules disagree: {out}"
        );
        assert!(
            !text.contains("trusted"),
            "the file keeps the one opinion it had: {text}"
        );
        assert_eq!(
            left.len(),
            1,
            "and the store copy is not deleted either — reconciling them is an edit for a human"
        );

        // The same refusal for the other way round: the name agrees, the
        // store copy reaches identities the file's does not.
        let users = [UserSeed {
            name: "nyx",
            role: "user",
            ids: &["nyx!~n@*.example.com", "telegram:77"],
        }];
        let (out, text, _, _) = promote_users(Some(toml), &[], &users, &["--users"]);
        assert!(
            out.contains("adds identities the file's user does not carry"),
            "{out}"
        );
        assert!(out.contains("telegram:77"), "{out}");
        assert!(!text.contains("telegram:77"), "{text}");
    }

    /// Promote writes grants. A store holding users and no `--users` says so
    /// rather than reporting "nothing to promote" over rules it walked past.
    #[test]
    fn runtime_users_are_left_alone_unless_asked() {
        let users = [UserSeed {
            name: "nyx",
            role: "trusted",
            ids: &["nyx!~n@*.example.com"],
        }];
        let (out, text, _, left) = promote_users(Some(CONFIGURED), &[], &users, &[]);
        assert!(out.contains("left in the store (1 runtime user)"), "{out}");
        assert!(
            out.contains("--users"),
            "and names the flag that would write it: {out}"
        );
        assert!(!text.contains("[[acl.users]]"), "{text}");
        assert_eq!(left.len(), 1);
    }

    /// `/acl export --write` is not a second writer into config.toml: it is
    /// `/acl promote` with the flags that mean "the whole store view", and it
    /// says which command it is. One writer, one set of refusals, one backup.
    #[test]
    fn export_write_is_promote_and_not_a_second_writer() {
        let grants = [Seed::new("bob!*@*", "trusted").ttl(3600)];
        let users = [UserSeed {
            name: "nyx",
            role: "trusted",
            ids: &["nyx!~n@*.example.com"],
        }];
        let (out, text) = acl_run(Some(CONFIGURED), &grants, &users, "export --write", None);
        assert!(
            out.contains("/acl promote --include-expiring --users"),
            "the report names the command it is: {out}"
        );
        assert!(
            out.contains("promoted 1 runtime grant and 1 runtime user into"),
            "{out}"
        );
        assert!(
            text.contains("[[acl.entries]]") && text.contains("[[acl.users]]"),
            "both shapes, one writer: {text}"
        );
        assert!(
            text.contains("expiry"),
            "export promised the whole view, so the expiring grant is written — and the file \
             says the expiry went: {text}"
        );

        // The flags mean nothing without --write, and saying so beats
        // printing a pasteable dump the operator did not ask for.
        let (out, text) = acl_run(Some(CONFIGURED), &grants, &users, "export --dry-run", None);
        assert!(out.contains("only mean something with --write"), "{out}");
        assert!(!text.contains("[[acl.entries]]"), "{text}");

        // Reading the live rules is an admin's; writing config.toml is the
        // owner's, however the request is spelled.
        let (out, text) = acl_run(
            Some(CONFIGURED),
            &grants,
            &users,
            "export --write",
            Some("irc:stranger"),
        );
        assert!(
            out.contains("/acl export --write"),
            "the refusal names what was refused: {out}"
        );
        assert!(!text.contains("[[acl.entries]]"), "{text}");
    }
}

#[cfg(test)]
mod gen_tests {
    use super::*;

    #[test]
    fn gen_parses_kinds_routes_and_flags() {
        // Kinds and their aliases.
        for (word, kind) in [
            ("image", GenKind::Image),
            ("img", GenKind::Image),
            ("video", GenKind::Video),
            ("vid", GenKind::Video),
            ("tts", GenKind::Audio),
            ("say", GenKind::Audio),
        ] {
            let r = parse_gen(&format!("{word} a red boat"))
                .unwrap_or_else(|| panic!("{word} must parse"));
            assert_eq!(r.kind, kind, "{word}");
            assert_eq!(r.prompt, "a red boat", "{word}");
            assert!(r.provider.is_none() && r.model.is_none(), "{word}");
        }
        // The route shorthand: bare comfyui, and provider/model where
        // the model is everything after the FIRST slash.
        let r = parse_gen("image comfyui a red boat").unwrap();
        assert_eq!(r.provider.as_deref(), Some("comfyui"));
        assert_eq!(r.prompt, "a red boat");
        let r = parse_gen("img router/google/image-x a red boat").unwrap();
        assert_eq!(r.provider.as_deref(), Some("router"));
        assert_eq!(r.model.as_deref(), Some("google/image-x"));
        assert_eq!(r.prompt, "a red boat");
        // A slash-word after the prompt STARTS is speech, not a route.
        let r = parse_gen("image a cat/dog diptych").unwrap();
        assert!(r.provider.is_none(), "mid-prompt slashes are words");
        assert_eq!(r.prompt, "a cat/dog diptych");
        // Flags, both spellings, plus the audio-only ones.
        let r = parse_gen("audio --voice alloy --speed 1.5 hello there").unwrap();
        assert_eq!(r.voice.as_deref(), Some("alloy"));
        assert_eq!(r.speed, Some(1.5));
        assert_eq!(r.prompt, "hello there");
        let r = parse_gen("image --provider=router --model=m hi").unwrap();
        assert_eq!(r.provider.as_deref(), Some("router"));
        assert_eq!(r.model.as_deref(), Some("m"));
        assert!(!r.list);
        assert!(parse_gen("image --list").unwrap().list);
        // No kind, no guess.
        assert!(
            parse_gen("draw me a cat").is_none(),
            "no kind word is usage, not a guess"
        );
        assert!(parse_gen("").is_none());
    }

    #[test]
    fn gen_is_gated_on_media_gen_which_is_a_user_cap() {
        // The ACL shape of USER's directive: generation is user+.
        // Guests are refused and told which capability would let them
        // in; users and owner surfaces pass; the cap, not the role, is
        // what speaks (an admin WITHOUT the cap is still refused).
        let mk = |role, caps| veles_core::acl::Principal {
            id: None,
            subject: "anon:#chan".into(),
            role,
            caps,
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };
        let guest = mk(
            veles_core::acl::Role::Guest,
            veles_core::acl::Role::Guest.default_caps(),
        );
        let d = gen_denied(Some(&guest)).expect("a guest must be refused");
        assert!(
            d.contains("media-gen"),
            "the denial names the capability: {d}"
        );

        let user = mk(
            veles_core::acl::Role::User,
            veles_core::acl::Role::User.default_caps(),
        );
        assert!(gen_denied(Some(&user)).is_none(), "users generate media");
        assert!(gen_denied(None).is_none(), "owner surfaces always pass");

        let adminless = mk(
            veles_core::acl::Role::Admin,
            veles_core::acl::Role::Guest.default_caps(),
        );
        assert!(
            gen_denied(Some(&adminless)).is_some(),
            "the cap decides, not the role string"
        );
    }

    #[test]
    fn a_generation_is_rate_limited_per_caller_and_the_ceiling_is_per_role() {
        // `/gen` is answered in the gateway's COMMAND region, which sits
        // above both turn brakes on purpose (so `/status` is never
        // billed). A generation is not `/status`: it is a provider bill
        // and a 300s HTTP request, and until 2026-09-11 a `user` could
        // run them in a loop, unbounded in rate AND in concurrency.
        //
        // USER set the numbers: "images are also super cheap so maybe
        // set the rate limit per hour caps at 40 user and unlimited
        // above."
        let (_home, _env) = veles_core::testenv::scratch_home();
        let limits = veles_core::acl::LimitsSection::default();
        assert_eq!(
            limits.media_per_hour_for_role(veles_core::acl::Role::User),
            40,
            "a user gets 40 generations an hour by default"
        );
        for role in [
            veles_core::acl::Role::Trusted,
            veles_core::acl::Role::Admin,
            veles_core::acl::Role::Owner,
        ] {
            assert_eq!(
                limits.media_per_hour_for_role(role),
                0,
                "{} is unlimited (0) — the cap is for strangers",
                role.as_str()
            );
        }
        // Video has its own bucket, defaulting to unlimited so this
        // change alters no behaviour — the dial exists because a clip is
        // ~1200 credits and `video-gen` starts at trusted, which the
        // media ceiling above leaves uncapped.
        for role in [veles_core::acl::Role::Trusted, veles_core::acl::Role::User] {
            assert_eq!(limits.video_per_hour_for_role(role), 0);
        }
        // And the brake itself: a ceiling of 2 refuses the third claim
        // for that subject, and a DIFFERENT subject is unaffected —
        // a per-caller budget, not a global one.
        let store =
            veles_core::store::Store::open(&veles_core::config::veles_home().join("state.db"))
                .unwrap();
        for i in 0..2 {
            assert!(
                store
                    .claim_rate(
                        veles_core::store::Store::MEDIA_RATE_PREFIX,
                        "irc:a",
                        2,
                        3600
                    )
                    .unwrap()
                    .is_none(),
                "claim {i} is inside the ceiling"
            );
        }
        let retry = store
            .claim_rate(
                veles_core::store::Store::MEDIA_RATE_PREFIX,
                "irc:a",
                2,
                3600,
            )
            .unwrap()
            .expect("the third claim is refused");
        assert!(retry > 0, "the refusal says when a slot frees up");
        assert!(
            store
                .claim_rate(
                    veles_core::store::Store::MEDIA_RATE_PREFIX,
                    "irc:b",
                    2,
                    3600
                )
                .unwrap()
                .is_none(),
            "one caller's flood must not refuse another"
        );
        // The buckets are separate: spending the media budget leaves the
        // turn budget alone, which is what makes them different knobs.
        assert!(store.claim_turn("irc:a", 2, 3600).unwrap().is_none());
    }

    #[test]
    fn the_audio_door_and_the_tts_tool_answer_to_one_cap() {
        // The bug this pins: `/gen audio` gated on `media-gen` and then
        // called `TtsTool` directly, while `text_to_speech` is
        // `Cap::Tts` — so a `user` was refused the tool and allowed the
        // command. USER 2026-09-11 settled which way to close it ("users
        // should have access to tts though because it's cheap"), so the
        // fix was to give User the cap AND to make the command ask for
        // it. Delete `audio_gate`'s call and this still passes; delete
        // the CAP from User and it goes red — which is the pairing that
        // matters, because the two must move together.
        let mk = |role: veles_core::acl::Role| veles_core::acl::Principal {
            id: None,
            subject: "anon:#chan".into(),
            role,
            caps: role.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };
        assert!(
            audio_gate(Some(&mk(veles_core::acl::Role::User))).is_none(),
            "tts is cheap and a user cap: /gen audio must pass"
        );
        assert!(audio_gate(Some(&mk(veles_core::acl::Role::Trusted))).is_none());
        assert!(audio_gate(None).is_none(), "owner surfaces always pass");
        // A guest holds neither cap, so both doors refuse — and the
        // refusal names the capability rather than the role.
        let guest = mk(veles_core::acl::Role::Guest);
        let d = audio_gate(Some(&guest)).expect("a guest must be refused audio");
        assert!(d.contains("tts"), "names the cap: {d}");
        assert!(gen_denied(Some(&guest)).is_some());
        // The two doors read ONE table. This is the assertion that goes
        // red if a future change retiers `text_to_speech` and forgets
        // the command.
        assert_eq!(
            veles_core::acl::tool_cap("text_to_speech"),
            Some(veles_core::acl::Cap::Tts)
        );
    }

    #[test]
    fn video_escalates_to_its_own_cap_which_a_user_does_not_have() {
        // USER 2026-09-11, after a 1200-credit clip: video is NOT the
        // media-gen floor — `/gen video` needs `video-gen`, which starts
        // at Trusted. A user generating an image must still pass the
        // same gate, so the two cannot be conflated.
        let mk = |role| veles_core::acl::Principal {
            id: None,
            subject: "anon:#chan".into(),
            role,
            caps: role.default_caps(),
            rule_context_limit: None,
            mask: None,
            via: None,
            ticket_role: None,
        };
        // A user passes the floor…
        assert!(gen_denied(Some(&mk(veles_core::acl::Role::User))).is_none());
        // …but is refused video, and told which capability would let it.
        let d = video_gate(Some(&mk(veles_core::acl::Role::User)))
            .expect("a user must be refused video");
        assert!(d.contains("video-gen"), "names the cap: {d}");
        // Trusted passes; owner surfaces always pass.
        assert!(video_gate(Some(&mk(veles_core::acl::Role::Trusted))).is_none());
        assert!(video_gate(None).is_none());
        // Both doors agree: the tool-layer mapping says the same thing
        // as the command's escalation, so `generate_video` (the model's
        // door) cannot slip under the floor the command enforces.
        assert_eq!(
            veles_core::acl::tool_cap("generate_video"),
            Some(veles_core::acl::Cap::VideoGen)
        );
        assert_eq!(
            veles_core::acl::tool_cap("generate_image"),
            Some(veles_core::acl::Cap::MediaGen)
        );
        // And the cap ladder is honest: User lacks video-gen, Trusted
        // has it, and media-gen is unchanged (users still make pictures).
        assert!(!veles_core::acl::Role::User
            .default_caps()
            .contains(&veles_core::acl::Cap::VideoGen));
        assert!(veles_core::acl::Role::Trusted
            .default_caps()
            .contains(&veles_core::acl::Cap::VideoGen));
        assert!(veles_core::acl::Role::User
            .default_caps()
            .contains(&veles_core::acl::Cap::MediaGen));
    }

    // ── /triggers log ────────────────────────────────────────────────

    /// A store in the scratch home (which the caller must already have
    /// created). `acl_listing_tests::seeded_store` does the same thing
    /// but lives in a sibling test module, so it is not reachable from
    /// here — and seeding ACL grants/users is not what these tests are
    /// about.
    fn trigger_store() -> std::sync::Arc<std::sync::Mutex<veles_core::store::Store>> {
        std::sync::Arc::new(std::sync::Mutex::new(
            veles_core::store::Store::open(&veles_core::config::veles_home().join("state.db"))
                .unwrap(),
        ))
    }

    fn trigger_log_ctx<'a>(
        store: &'a std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
        fallback: &'a veles_core::acl::AclConfig,
    ) -> AclCtx<'a> {
        AclCtx {
            store,
            fallback,
            session_id: "",
            principal: None,
            sender: None,
        }
    }

    /// Seed `name` and record `fires` against it: `(status, detail)` per
    /// row, oldest first.
    fn seed_triggers_with_fires(
        store: &std::sync::Arc<std::sync::Mutex<veles_core::store::Store>>,
        name: &str,
        fires: &[(&str, Option<&str>)],
    ) {
        let row = veles_core::store::TriggerRow {
            id: 0,
            name: name.to_string(),
            enabled: true,
            platform: Some("irc".into()),
            network: Some("irc.synirc.net".into()),
            channel: Some("#flashsupport".into()),
            event: "TEXT".into(),
            match_kind: "always".into(),
            pattern: None,
            action: "slopdrop".into(),
            arg: None,
            cooldown_secs: 0,
            per_hour_cap: 0,
            min_role: None,
            fire_count: 0,
            last_fired_at: None,
            last_status: None,
        };
        let id = store.lock_ok().trigger_upsert(&row).unwrap();
        for (status, detail) in fires {
            store
                .lock_ok()
                .trigger_record_fire(id, "#flashsupport", status, *detail)
                .unwrap();
        }
    }

    /// C1 — a multi-word `arg=` was untypeable from chat AND the CLI,
    /// so both live vision rows (each carrying an English question) had
    /// to be created by hand through REST JSON. `split_whitespace` was
    /// the whole parser on the add path, and the edit arm then re-joined
    /// its own tokens with spaces, destroying the quoting a second time
    /// inside the surface meant to honour it.
    #[test]
    fn a_multi_word_value_is_typeable_from_the_chat_surface() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let store = trigger_store();
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = trigger_log_ctx(&store, &fallback);
        let question = "Describe this image in one or two sentences for an IRC channel.";

        let out = triggers_command(
            &ctx,
            &format!(
                "add coven-vision network=irc.IRC4Fun.net channel=#coven \
                 match=imageurl action=vision arg=\"{question}\""
            ),
        );
        assert!(out.contains("added"), "{out}");
        let row = store
            .lock_ok()
            .trigger_get("coven-vision")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.arg.as_deref(),
            Some(question),
            "the whole sentence, not its first word"
        );
        assert_eq!(
            row.channel.as_deref(),
            Some("#coven"),
            "and the rest parsed"
        );

        // EDIT: the arm used to re-join `parts` with spaces before this
        // ever reached a parser. The quotes survived that, so the value
        // under test carries INTERIOR whitespace — two spaces and a tab
        // — which a split/join round trip silently flattens.
        let spaced = "a  different\tquestion entirely";
        let out = triggers_command(&ctx, &format!("edit coven-vision arg='{spaced}'"));
        assert!(out.contains("updated"), "{out}");
        let row = store
            .lock_ok()
            .trigger_get("coven-vision")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.arg.as_deref(),
            Some(spaced),
            "a quoted value is stored verbatim — the arm must not reformat it"
        );

        // An unterminated quote is NAMED, and writes nothing — a value
        // silently cut in half is the failure this exists to end.
        let out = triggers_command(&ctx, "add halfway arg=\"never closed");
        assert!(out.contains("unterminated"), "{out}");
        assert!(store.lock_ok().trigger_get("halfway").unwrap().is_none());
    }

    /// A3 — `/triggers add` swallowed every k=v key it did not
    /// recognise, so one typo in `network=` produced a row scoped to
    /// ANY network, and `/triggers edit` checked no VALUES at all, so
    /// `match=contians` reached the store and read back as `Always` —
    /// an image-URL row silently widened to every line in the channel.
    /// Four surfaces, four different subsets; one validator now.
    #[test]
    fn a_trigger_write_refuses_an_unknown_key_or_an_unparseable_value() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let store = trigger_store();
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = trigger_log_ctx(&store, &fallback);

        // A misspelled KEY is not a row with a missing field, it is a
        // row scoped wider than the operator typed.
        let out = triggers_command(&ctx, "add wide netwrok=irc.synirc.net channel=#c");
        assert!(out.contains("netwrok"), "names the bad key: {out}");
        assert!(
            store.lock_ok().trigger_get("wide").unwrap().is_none(),
            "a refused add must not have written the row"
        );

        // A misspelled min-role is the one that fails OPEN: an
        // unparseable floor reads back as Guest.
        let out = triggers_command(
            &ctx,
            "add gated network=irc.synirc.net channel=#c min-role=ownr",
        );
        assert!(out.contains("ownr"), "names the bad role: {out}");
        assert!(store.lock_ok().trigger_get("gated").unwrap().is_none());

        // A good row still lands.
        let out = triggers_command(
            &ctx,
            "add good network=irc.synirc.net channel=#c match=imageurl action=vision \
             min-role=trusted",
        );
        assert!(out.contains("added"), "{out}");

        // …and edit must not be able to widen it afterwards.
        for (rest, bad) in [
            ("edit good match=contians", "contians"),
            ("edit good action=visoin", "visoin"),
            ("edit good event=MSG", "MSG"),
            ("edit good min-role=ownr", "ownr"),
        ] {
            let out = triggers_command(&ctx, rest);
            assert!(out.contains(bad), "edit must refuse '{bad}': {out}");
        }
        let row = store.lock_ok().trigger_get("good").unwrap().unwrap();
        assert_eq!(row.match_kind, "imageurl", "no refused edit landed");
        assert_eq!(row.action, "vision");
        assert_eq!(row.event, "TEXT");
        assert_eq!(row.min_role.as_deref(), Some("trusted"));
    }

    /// `/triggers log 20` asks for TWENTY ROWS. The arm read its second
    /// token as the trigger NAME and only the third as the limit, so a
    /// bare number became a filter for a trigger called "20" — and the
    /// empty result printed as "no fires recorded", which is the same
    /// sentence a trigger that genuinely never fired produces. Found
    /// live: `/triggers log 20` → "no fires recorded" while the rows
    /// beside it reported fires: 13.
    #[test]
    fn a_bare_number_after_log_is_a_limit_not_a_trigger_name() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let store = trigger_store();
        seed_triggers_with_fires(
            &store,
            "slop-flashsupport",
            &[
                ("refused", Some("mask: event not wanted by this channel")),
                ("fired", None),
                ("fired", None),
            ],
        );
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = trigger_log_ctx(&store, &fallback);

        let out = triggers_command(&ctx, "log 2");
        assert!(
            out.contains("last 2 fire(s)"),
            "a bare number is the LIMIT: {out}"
        );
        // Two of three, newest first — the limit took effect on the
        // whole log, not on a name filter that matched nothing.
        let lines: Vec<&str> = out.lines().skip(1).collect();
        assert_eq!(lines.len(), 2, "exactly the limit's worth of rows: {out}");
        assert!(lines[0].contains("fired"), "newest first: {out}");

        // And the un-limited form still reports everything.
        let all = triggers_command(&ctx, "log");
        assert!(
            all.contains("last 3 fire(s)"),
            "no argument = the default window over all triggers: {all}"
        );
    }

    /// The log exists to answer "what did it do and WHY not". A refusal
    /// row carries the reason in `detail` and the arm dropped it, so the
    /// nine mask refusals that diagnosed the crate-boundary bug were
    /// unreadable from the surface that recorded them — `/triggers log`
    /// said only "refused" where the REST history said "mask: event not
    /// wanted by this channel".
    #[test]
    fn a_refusal_says_why_it_refused() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let store = trigger_store();
        seed_triggers_with_fires(
            &store,
            "slop-flashsupport",
            &[("refused", Some("mask: event not wanted by this channel"))],
        );
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = trigger_log_ctx(&store, &fallback);

        let out = triggers_command(&ctx, "log");
        assert!(
            out.contains("mask: event not wanted by this channel"),
            "the reason is the point of the row: {out}"
        );
    }

    /// "no fires recorded" must mean what it says. A name that does not
    /// exist is a different answer — a typo on this surface looks
    /// identical to a healthy trigger that has never fired, and the
    /// user's next move is to go re-check the trigger instead of their
    /// spelling.
    #[test]
    fn an_unknown_trigger_name_says_so_instead_of_claiming_no_fires() {
        let (_home, _guard) = veles_core::testenv::scratch_home();
        let store = trigger_store();
        seed_triggers_with_fires(&store, "slop-flashsupport", &[("fired", None)]);
        let fallback = veles_core::acl::AclConfig::default();
        let ctx = trigger_log_ctx(&store, &fallback);

        let out = triggers_command(&ctx, "log nosuchtrigger");
        assert!(
            out.contains("no trigger named 'nosuchtrigger'"),
            "an unknown name is named, not reported as empty: {out}"
        );
        assert!(
            !out.contains("no fires recorded"),
            "and it does not borrow the healthy-empty sentence: {out}"
        );

        // The real name still gets its rows, so the check is on the
        // name and not on the whole arm.
        let real = triggers_command(&ctx, "log slop-flashsupport");
        assert!(
            real.contains("slop-flashsupport") && real.contains("fired"),
            "a known name lists its fires: {real}"
        );
    }
}

/// The collection just above ends with the "an unnamed provider gets
/// no plan" test; the report tests live beside it.
#[cfg(test)]
mod usage_report_tests {
    use super::*;

    /// Seed the ledger the split reads: four plans plus a provider no
    /// plan claims.
    fn seed(mock: &Agent) {
        let st = mock.store.clone();
        let s = st.lock().unwrap();
        s.create_session("usage-report").expect("session");
        let tok = veles_core::store::CallTokens {
            prompt_tokens: 1000,
            completion_tokens: 100,
            cached_tokens: 0,
            cache_write_tokens: 0,
            cache_reported: false,
            reasoning_tokens: None,
        };
        for (prov, n) in [
            ("ali-token-plan-personal", 3),
            ("ali-token-plan-team", 2),
            ("ali-responses", 1),
            ("kimi-coding-plan", 2),
            ("openrouter", 1),
        ] {
            for _ in 0..n {
                s.record_usage("usage-report", prov, "m", tok)
                    .expect("usage");
            }
        }
    }

    /// THE COMPLAINT ON THE TEXT SURFACES TOO. `/usage` reported one
    /// total across every provider — "1.9G tok" spanning the personal
    /// plan, the team plan, z.ai and Kimi — which is exactly the
    /// conflation being complained about. The report now carries the
    /// split, with each row naming its plan AND the providers that fed
    /// it, so the grouping can be checked rather than trusted.
    #[tokio::test]
    async fn the_usage_report_splits_the_ledger_by_plan() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "usage-report");
        seed(&agent);

        let out = usage_report(&agent);

        assert!(out.contains("by plan (all sessions):"), "{out}");
        // Team covers BOTH team entries — the conflation case.
        assert!(
            out.contains("team") && out.contains("ali-responses, ali-token-plan-team"),
            "the team row names both providers that feed it: {out}"
        );
        assert!(out.contains("personal"), "{out}");
        assert!(out.contains("kimi"), "{out}");
        // The unclaimed provider keeps its own row rather than being
        // folded into a plan it does not belong to.
        assert!(
            out.contains("openrouter"),
            "an unmapped provider is named, not hidden in an 'other' \
             bucket: {out}"
        );

        // The session's own split is there too, and it is scoped.
        assert!(out.contains("by plan (this session):"), "{out}");

        // …and the per-provider cache rows survive: the split is an
        // addition, not a replacement.
        assert!(
            out.contains("ali-token-plan-personal/m"),
            "the per-provider cache breakdown is still reported: {out}"
        );
    }

    /// The split is a GROUPING, and a grouping that silently loses a
    /// provider is worse than no grouping. Every call recorded must be
    /// accounted for by some row.
    #[tokio::test]
    async fn the_split_accounts_for_every_call_it_was_given() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "usage-total");
        seed(&agent);

        let store = agent.store.lock_ok();
        let rows = store.usage_by_provider_scoped(None).expect("ledger");
        let total: u64 = rows.iter().map(|r| r.calls).sum();
        let split: u64 = veles_core::tokenplan::ledger_by_plan(&rows)
            .iter()
            .map(|r| r.calls)
            .sum();
        assert_eq!(
            total, split,
            "every call the store holds must land in exactly one row: \
             {rows:#?}"
        );
        assert_eq!(total, 9, "3+2+1+2+1, the fixture's own count");
    }
}

/// `/quota` — the tier is a line through the CONTENT, and the verbs
/// route without a socket because every one of them reads the cache.
#[cfg(test)]
mod quota_command_tests {
    use super::*;
    use veles_core::tokenplan::{Credits, QuotaSnapshot, Seat, Spend, TeamQuota};

    fn enabled_cfg() -> veles_core::config::VelesConfig {
        let mut cfg = veles_core::config::VelesConfig::default();
        cfg.tokenplan.enabled = true;
        cfg
    }

    fn seeded_snapshot() -> QuotaSnapshot {
        QuotaSnapshot {
            fetched_at: chrono::Utc::now(),
            team: Some(TeamQuota {
                seats: vec![Seat {
                    seat_id: Some("seat_e5cfa7b0".into()),
                    subscription: Some("subs-42217ce2C4tFHvOx".into()),
                    spec_type: Some("max".into()),
                    status: Some("NORMAL".into()),
                    assigned: Some("ASSIGNED".into()),
                    credits: Some(Credits {
                        total: Some(250000.0),
                        remaining: Some(111650.32),
                        unit: "Credit".into(),
                        cycle_start: None,
                        cycle_end: None,
                    }),
                }],
                members: Some(1),
                seated: Some(1),
                unseated: Some(0),
                spend: Some(Spend {
                    bill_month: "2026-09".into(),
                    pretax: "206.45".into(),
                    tax: "47.48".into(),
                    total: "253.93".into(),
                    currency: "USD".into(),
                }),
            }),
            team_error: None,
            personal: None,
            personal_error: Some("console endpoints not derived yet".into()),
            zai: None,
            zai_error: None,
            kimi: None,
            kimi_error: None,
        }
    }

    fn ctx_for<'a>(
        agent: &'a Agent,
        principal: Option<&'a veles_core::acl::Principal>,
    ) -> AclCtx<'a> {
        AclCtx::from_agent(agent, principal)
    }

    /// A guest may learn how much of the plan is left — that is the
    /// whole point of the command, and refusing it only means they find
    /// out by being refused mid-turn. What they may not learn is which
    /// account is behind the agent.
    #[tokio::test]
    async fn the_bare_form_answers_a_guest_with_pools_but_no_account() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-guest");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot());

        let guest = veles_core::acl::Principal::known("irc:stranger", veles_core::acl::Role::Guest);
        let out = quota_command(&ctx_for(&agent, Some(&guest)), &enabled_cfg(), "", None).await;

        assert!(
            out.contains("55.3% used"),
            "the quota figure is open: {out}"
        );
        assert!(out.contains("111,650 of 250,000"), "{out}");
        for withheld in ["seat_e5cfa7b0", "subs-42217ce2C4tFHvOx", "253.93"] {
            assert!(
                !out.contains(withheld),
                "{withheld:?} names the account and must not reach a guest: {out}"
            );
        }
        veles_core::tokenplan::invalidate();
    }

    /// Every verb past the bare form names the account or spends a
    /// remote call, so every one of them takes cmd-admin — and says so
    /// by name rather than answering an empty section.
    #[tokio::test]
    async fn every_detail_verb_is_refused_below_cmd_admin_by_name() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-deny");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot());

        let guest = veles_core::acl::Principal::known("irc:stranger", veles_core::acl::Role::Guest);
        for verb in ["team", "personal", "zai", "kimi", "spend", "refresh"] {
            let out =
                quota_command(&ctx_for(&agent, Some(&guest)), &enabled_cfg(), verb, None).await;
            assert!(
                out.contains("[acl]") && out.contains("cmd-admin"),
                "/quota {verb} must be refused by name: {out}"
            );
            assert!(
                !out.contains("111,650"),
                "/quota {verb} must not leak the report while refusing: {out}"
            );
        }
        veles_core::tokenplan::invalidate();
    }

    /// An owner surface passes no principal and gets the whole thing,
    /// sliced to the section it asked for — through the ONE formatter,
    /// so the section cannot drift from the full report.
    #[tokio::test]
    async fn an_owner_gets_the_account_and_one_section_at_a_time() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-owner");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot());
        let cfg = enabled_cfg();

        let team = quota_command(&ctx_for(&agent, None), &cfg, "team", None).await;
        assert!(team.contains("seat_e5cfa7b0"), "{team}");
        assert!(
            team.contains("253.93"),
            "spend is in the team section: {team}"
        );
        assert!(
            team.contains("plan quota"),
            "a section keeps the header that says how old it is: {team}"
        );
        assert!(
            !team.contains("console endpoints"),
            "…and carries only the section asked for: {team}"
        );

        let personal = quota_command(&ctx_for(&agent, None), &cfg, "personal", None).await;
        assert!(
            personal.contains("unavailable — console endpoints not derived yet"),
            "the personal section says why it is empty: {personal}"
        );
        assert!(!personal.contains("111,650"), "{personal}");

        // `/quota kimi` — the fourth plan, added 2026-09-20. It goes
        // through the same ONE formatter and the same section slicer as
        // the other three, so a section cannot drift from the full
        // report.
        let mut snap = seeded_snapshot();
        snap.kimi = Some(veles_core::tokenplan::KimiQuota {
            windows: vec![
                veles_core::tokenplan::Window {
                    label: "5-hour".into(),
                    used_fraction: Some(1.0),
                    resets_at: None,
                },
                veles_core::tokenplan::Window {
                    label: "month".into(),
                    used_fraction: Some(0.0546),
                    resets_at: None,
                },
            ],
        });
        veles_core::tokenplan::seed_cache_for_test(snap);
        let kimi = quota_command(&ctx_for(&agent, None), &cfg, "kimi", None).await;
        assert!(kimi.contains("5-hour    100.0% used"), "{kimi}");
        assert!(kimi.contains("month     5.5% used"), "{kimi}");
        assert!(
            kimi.contains("plan quota"),
            "a section keeps the header that says how old it is: {kimi}"
        );
        assert!(
            !kimi.contains("111,650"),
            "…and carries only the section asked for: {kimi}"
        );
        veles_core::tokenplan::invalidate();
    }

    /// Off by default, and the refusal names the key that turns it on
    /// rather than answering with an empty report.
    #[tokio::test]
    async fn a_disabled_tokenplan_names_the_config_key() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-off");
        let out = quota_command(
            &ctx_for(&agent, None),
            &veles_core::config::VelesConfig::default(),
            "",
            None,
        )
        .await;
        assert!(out.contains("[tokenplan] disabled"), "{out}");
        assert!(
            out.contains("enabled = true"),
            "and how to turn it on: {out}"
        );
    }

    /// A malformed month is refused before it becomes a signed request
    /// against a parameter the far side will reject opaquely.
    #[test]
    fn a_bill_month_must_be_yyyy_dash_mm() {
        assert!(month_is_well_formed("2026-09"));
        assert!(month_is_well_formed("2026-12"));
        assert!(!month_is_well_formed("2026-13"));
        assert!(!month_is_well_formed("2026-00"));
        assert!(!month_is_well_formed("2026-9"));
        assert!(!month_is_well_formed("26-09"));
        assert!(!month_is_well_formed("september"));
        assert!(!month_is_well_formed(""));
    }

    /// THE RELEVANCE CONTRACT (USER, 2026-09-16). The three plans are
    /// disjoint pools: showing a session what is left on a plan it is
    /// not on answers the wrong question with a confident number. When
    /// the surface knows the active provider, its own plan is marked
    /// `← active` and only that section carries the marker.
    #[tokio::test]
    async fn the_session_provider_marks_its_own_plan_and_no_other() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-active");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot_with_zai());
        let cfg = enabled_cfg();

        let out = quota_command(
            &ctx_for(&agent, None),
            &cfg,
            "",
            Some("custom:zai-coding-plan"),
        )
        .await;
        assert!(
            out.contains("zai ← active"),
            "the session's own plan is marked: {out}"
        );
        assert!(
            out.matches("← active").count() == 1,
            "exactly one section is the session's: {out}"
        );
        assert!(
            out.contains("60.0% used"),
            "the zai weekly figure is in the report: {out}"
        );

        // …and the marker MOVES with the provider: a team session is
        // marked on team, not on zai.
        let out = quota_command(
            &ctx_for(&agent, None),
            &cfg,
            "",
            Some("ali-token-plan-team"),
        )
        .await;
        assert!(out.contains("team ← active"), "{out}");
        assert!(!out.contains("zai ← active"), "{out}");
        veles_core::tokenplan::invalidate();
    }

    /// `/quota zai` answers the z.ai section alone, through the same
    /// one-render-then-slice path as team and personal.
    #[tokio::test]
    async fn quota_zai_slices_the_zai_section() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-zai");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot_with_zai());

        let out = quota_command(&ctx_for(&agent, None), &enabled_cfg(), "zai", None).await;
        assert!(out.contains("zai"), "{out}");
        assert!(out.contains("60.0% used"), "{out}");
        assert!(out.contains("search-prime 54"), "{out}");
        assert!(
            !out.contains("111,650"),
            "the zai section must not carry the team pool: {out}"
        );
        veles_core::tokenplan::invalidate();
    }

    /// The z.ai verb takes the same tier as every other verb: it names
    /// a vendor account, and a guest is refused by name.
    #[tokio::test]
    async fn quota_zai_is_admin_gated_like_the_rest() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = veles_core::testenv::mock_agent("http://127.0.0.1:0", "quota-zaigate");
        veles_core::tokenplan::seed_cache_for_test(seeded_snapshot_with_zai());
        let guest = veles_core::acl::Principal::known("irc:stranger", veles_core::acl::Role::Guest);
        let out = quota_command(&ctx_for(&agent, Some(&guest)), &enabled_cfg(), "zai", None).await;
        assert!(
            out.contains("cmd-admin"),
            "/quota zai must be refused by name: {out}"
        );
        veles_core::tokenplan::invalidate();
    }

    fn seeded_snapshot_with_zai() -> QuotaSnapshot {
        let mut snap = seeded_snapshot();
        snap.zai = Some(veles_core::tokenplan::ZaiQuota {
            level: Some("pro".into()),
            window_5h: Some(veles_core::tokenplan::Window {
                label: "5-hour".into(),
                used_fraction: Some(0.01),
                resets_at: None,
            }),
            weekly: Some(veles_core::tokenplan::Window {
                label: "weekly".into(),
                used_fraction: Some(0.60),
                resets_at: None,
            }),
            tools: Some(veles_core::tokenplan::ZaiTools {
                limit: Some(1000.0),
                used: Some(69.0),
                remaining: Some(931.0),
                resets_at: None,
                details: vec![("search-prime".into(), 54.0), ("web-reader".into(), 15.0)],
            }),
        });
        snap
    }
}
