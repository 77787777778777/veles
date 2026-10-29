//! CLI frontend — REPL + one-shot + doctor.

use anyhow::Result;
use std::sync::{Arc, Mutex};
use veles_core::agent::Agent;
use veles_core::config::load;
use veles_core::hooks::HookBus;
use veles_core::lockutil::LockOk;
use veles_core::providers::StreamEvent;
use veles_core::store::Store;
use veles_core::tclrt;
use veles_core::tools;

pub struct Boot {
    pub agent: Agent,
    pub cfg: Arc<veles_core::config::VelesConfig>,
}

/// Shared boot path for REPL, one-shot, TUI: config + Tcl + hooks + tools +
/// store + agent, with session resume. `explicit` (a session id or prefix)
/// wins over `continue_latest`.
pub fn boot_with(resume: bool, explicit: Option<&str>) -> Result<Boot> {
    let cfg = Arc::new(load()?);
    // i18n: install the process locale (config > env > system > en).
    let locale = veles_core::i18n::init(cfg.tui.locale.as_deref());
    tracing::debug!("locale: {locale}");
    let home = veles_core::config::veles_home();
    let tcl = tclrt::spawn(veles_core::config::config_dir())?;
    let hooks = HookBus::new(Some(tcl));
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    let session_id = if let Some(id) = explicit {
        // Full id or prefix match (hermes --resume parity), over every
        // session — not the newest 1,000.
        match store.lock_ok().find_session(id)? {
            Some(full) => full,
            None => anyhow::bail!("no session matching '{id}'"),
        }
    } else if resume {
        let latest = store.lock_ok().latest_session()?;
        latest.unwrap_or_else(new_session_id)
    } else {
        new_session_id()
    };
    store.lock_ok().create_session(&session_id)?;

    // Built after the session id is known: `todo` and `clarify` are
    // session-scoped tools.
    let registry = Arc::new(tools::builtin_registry(store.clone(), None, &session_id));
    let agent = Agent::new(cfg.clone(), hooks, registry, store, session_id)?;
    register_mcp_deferred(&cfg);
    Ok(Boot { agent, cfg })
}

/// Connect configured MCP servers and register their tools into the
/// deferred registry (gateway parity for the chat/TUI/one-shot paths),
/// with all MCP I/O on the process-lifetime MCP host thread.
/// Per-server failures are logged and skipped — MCP must never keep a
/// frontend from booting.
pub(crate) fn register_mcp_deferred(cfg: &veles_core::config::VelesConfig) {
    veles_core::mcp::boot_mcp_deferred(cfg);
}

/// Back-compat wrapper (existing callers pass a bool only).
pub fn boot(resume: bool) -> Result<Boot> {
    boot_with(resume, None)
}

fn new_session_id() -> String {
    use chrono::Local;
    format!(
        "{}_{}",
        Local::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..6]
    )
}

/// Print one stream event to stdout (CLI rendering).
/// a11y: VELES_PLAIN=1 swaps glyphs for semantic [tool] text markers —
/// screen-reader friendly and greppable.
pub fn print_event(ev: StreamEvent) {
    let plain = std::env::var("VELES_PLAIN")
        .map(|v| v == "1")
        .unwrap_or(false);
    match &ev {
        StreamEvent::Route {
            provider,
            model,
            reasoning_effort: _,
        } => eprintln!("[route: {provider}/{model}]"),
        StreamEvent::Delta(t) => {
            use std::io::Write;
            print!("{t}");
            let _ = std::io::stdout().flush();
        }
        StreamEvent::ReasoningDelta(_) => {} // hidden in plain CLI
        StreamEvent::ToolStart(name, _) => {
            if plain {
                eprintln!("  [tool] {name} starting");
            } else {
                eprintln!("  ⚙ {name}…");
            }
        }
        StreamEvent::ToolEnd(name, result) => {
            if plain {
                eprintln!("  [tool] {name} done ({} bytes)", result.len());
            } else {
                eprintln!("  ✓ {name} ({} bytes)", result.len());
            }
        }
        // One-shot CLI has no human attached: the approval gate answers
        // from `approvals.non_interactive` instead of asking.
        StreamEvent::Ask(_) => {}
        StreamEvent::Done(_) => println!(),
        StreamEvent::Cancelled => eprintln!("\n[interrupted]"),
        // Implicit actions are printed, always — the whole point is
        // that they stop being invisible. Detail follows indented so
        // one line stays one line.
        StreamEvent::Note(n) => {
            if plain {
                eprintln!("  [{}] {}", n.kind.as_str(), n.summary);
            } else {
                eprintln!("  {}", n.line());
            }
            if !n.detail.is_empty() {
                for l in n.detail.lines() {
                    eprintln!("      {l}");
                }
            }
            if let Some(t) = &n.toggle {
                eprintln!("      ({} = {} — {})", t.key, t.value, t.help);
            }
        }
    }
}

// ── S5: headless stream-json ─────────────────────────────────────────
//
// `veles chat -qj "<query>"` — line-delimited JSON events on stdout,
// JSON control lines on stdin. Scripting parity with Letta's
// stream-json: the parent process drives the turn, answers approvals/
// clarifies, and reads token/stream events back. The child (`-Q -q`)
// contract is untouched; this is the interactive-but-machine surface.

pub use veles_core::providers::json_event;

/// An Asker that renders the question as JSON on stdout and waits for a
/// JSON control line on stdin: `{"choice":"once"}`, `{"text":"..."}` or
/// (multi/multi-question asks) `{"answers":[...]}` /
/// `{"answers":{"q1":...}}`. Times out per the request's timeout_secs.
/// Reads raw stdin per ask so the handle is Send and never crosses
/// threads as a guard.
#[derive(Default)]
struct JsonAsker;

#[async_trait::async_trait]
impl veles_core::ask::Asker for JsonAsker {
    fn interactive(&self) -> bool {
        true
    }
    async fn ask(&self, req: veles_core::ask::AskRequest) -> veles_core::ask::AskAnswer {
        // Render + flush so the driver sees the question immediately.
        use std::io::Write;
        println!(
            "{}",
            serde_json::json!({
                "event": "ask",
                "id": req.id,
                "kind": match req.kind {
                    veles_core::ask::AskKind::Approval => "approval",
                    veles_core::ask::AskKind::Clarify => "clarify",
                    veles_core::ask::AskKind::Failover => "failover",
                    veles_core::ask::AskKind::Secret => "secret",
                    veles_core::ask::AskKind::Ticket => "ticket",
                    veles_core::ask::AskKind::Challenge => "challenge",
                },
                "prompt": req.prompt,
                "detail": req.detail,
                "choices": req.choices,
                "multi": req.multi,
                "questions": req.questions,
            })
        );
        let _ = std::io::stdout().flush();
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_secs(req.timeout_secs.max(5));
        loop {
            let line = veles_core::rt::spawn_blocking(|| {
                let mut l = String::new();
                let _ = std::io::stdin().read_line(&mut l);
                l
            })
            .await
            .unwrap_or_default();
            if line.trim().is_empty() {
                if tokio::time::Instant::now() >= deadline {
                    let _ =
                        veles_core::ask::resolve(&req.id, veles_core::ask::AskAnswer::timeout());
                    return veles_core::ask::AskAnswer::timeout();
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            // A line that is not an answer is SAID to be one, and the
            // ask keeps waiting for a good one — it used to resolve as a
            // timeout, so the model heard "nobody answered".
            let answer = match json_reply(&req, &line) {
                Ok(a) => a,
                Err(e) => {
                    println!(
                        "{}",
                        serde_json::json!({"event": "ask_error", "id": req.id, "error": e})
                    );
                    let _ = std::io::stdout().flush();
                    continue;
                }
            };
            let _ = veles_core::ask::resolve(&req.id, answer.clone());
            return answer;
        }
    }
}

/// One stdin control line → the answer it gives, or why it gives none.
/// `{"choice":…}`, `{"text":…}` and `{"answers":…}` are the driver's
/// shapes; anything else — bare words, "1,3", or a JSON reply object
/// such as {"q1":"1"} — goes through the reply grammar.
fn json_reply(
    req: &veles_core::ask::AskRequest,
    line: &str,
) -> Result<veles_core::ask::AskAnswer, String> {
    let line = line.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
        if let Some(a) = v.get("answers") {
            return veles_core::ask::answers_value(a).ok_or_else(|| {
                "bad `answers`: a list of choice keys, or {\"q1\": {\"selected\": [...], \
\"text\": \"...\"}}"
                    .to_string()
            });
        }
        if let Some(c) = v.get("choice").and_then(serde_json::Value::as_str) {
            return Ok(veles_core::ask::AskAnswer::choice(c));
        }
        if let Some(t) = v.get("text").and_then(serde_json::Value::as_str) {
            return Ok(veles_core::ask::AskAnswer::text(t));
        }
    }
    veles_core::ask::parse_reply(req, line).ok_or_else(|| {
        "not an answer to this ask: send {\"choice\": …}, {\"text\": …}, {\"answers\": …} \
or a reply the prompt names"
            .to_string()
    })
}

/// Headless one-shot with NDJSON I/O (S5). stdout carries stream events,
/// stdin carries approval/clarify/failover answers.
pub fn run_oneshot_json(
    query: &str,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
    continue_latest: bool,
    explicit_session: Option<&str>,
) -> Result<()> {
    // JSON mode always goes to stderr for logs; stdout is reserved for
    // the event stream (machine-readable).
    veles_core::logbuf::init(veles_core::logbuf::LogMode::Stderr);
    let rt = tokio::runtime::Runtime::new()?;
    let resume = continue_latest || explicit_session.is_some();
    let boot = boot_with(resume, explicit_session)?;
    // JSON mode: stdout is the event stream, so warnings go to stderr.
    for w in boot
        .agent
        .apply_overrides(&veles_core::agent::ModelOverrides::from_flags(
            model, provider, reasoning,
        ))
    {
        eprintln!("warning: {w}");
    }
    let mut history = if resume {
        boot.agent
            .store
            .lock()
            .unwrap()
            .load_history(&boot.agent.session_id)?
    } else {
        Vec::new()
    };
    // Wire the JSON asker: stdin control lines answer approvals,
    // clarifies, and failover questions emitted as `ask` events.
    boot.agent.set_asker(std::sync::Arc::new(JsonAsker));

    spawn_interrupt_handler(&rt, &boot.agent.session_id);
    let tr = rt.block_on(boot.agent.run_turn(&mut history, query, None, |ev| {
        if !matches!(&ev, StreamEvent::Ask(_)) {
            println!("{}", json_event(&ev));
            use std::io::Write as _;
            let _ = std::io::stdout().flush();
        }
    }))?;
    println!(
        "{}",
        serde_json::json!({
            "event": "result",
            "reply": tr.reply,
            "model": tr.model,
            "provider": tr.provider,
            "tool_rounds": tr.tool_rounds,
            "prompt_tokens": tr.prompt_tokens,
            "completion_tokens": tr.completion_tokens,
            // null = provider reports no cache field (unknown, not 0).
            "cached_tokens": tr.cached_tokens,
            "uncached_prompt_tokens": tr.uncached_prompt_tokens(),
        })
    );
    Ok(())
}

pub fn run_oneshot(
    query: &str,
    model: Option<&str>,
    provider: Option<&str>,
    reasoning: Option<&str>,
    quiet: bool,
    continue_latest: bool,
    explicit_session: Option<&str>,
) -> Result<()> {
    // Quiet mode is the machine-readable contract for delegate_task
    // children: stdout carries ONLY the final reply, so logs go to
    // stderr. `init` is idempotent — main.rs may already have installed
    // a subscriber, and this used to be a hard panic
    // ("a global default trace dispatcher has already been set") that
    // killed every non-quiet `chat -q` run.
    veles_core::logbuf::init(if quiet {
        veles_core::logbuf::LogMode::Stderr
    } else {
        veles_core::logbuf::LogMode::Stdout
    });
    let rt = tokio::runtime::Runtime::new()?;
    let resume = continue_latest || explicit_session.is_some();
    let boot = boot_with(resume, explicit_session)?;
    // Session-scoped overrides, mirroring the REPL's /model + /reasoning.
    for w in boot
        .agent
        .apply_overrides(&veles_core::agent::ModelOverrides::from_flags(
            model, provider, reasoning,
        ))
    {
        eprintln!("warning: {w}");
    }
    // Resumed sessions load their history so the turn has context
    // (and so compaction can act on it). Fixed 2026-08-10: the -q path
    // previously always started a fresh session, dropping --resume.
    let mut history = if resume {
        boot.agent
            .store
            .lock()
            .unwrap()
            .load_history(&boot.agent.session_id)?
    } else {
        Vec::new()
    };
    let printer = if quiet { quiet_event } else { print_event };
    spawn_interrupt_handler(&rt, &boot.agent.session_id);
    // A SLASH COMMAND in the one-shot path is dispatched, not billed:
    // the REPL always ran `/model` and friends without a turn, but
    // `chat -q "/lint f.tcl"` fell through to the provider — a model
    // turn (and a model ANSWER) for what is a local command. Found
    // live while proving /lint: the model offered to write the script.
    if query.starts_with('/') {
        let mut parts = query.splitn(2, ' ');
        let head = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("").trim();
        let a = &boot.agent;
        let mut h = history.clone();
        let mut ctx = crate::commands::CommandCtx {
            agent: a,
            surface: crate::commands::Surface::Repl,
            history: Some(&mut h),
            turn_live: false,
            principal: None,
        };
        // Sync dispatcher first; its async twin covers the few arms
        // that need it (/sandbox, /gen, /tcl...).
        let handled = crate::commands::dispatch(&mut ctx, head, rest);
        let handled = match handled {
            Some(h) => Some(h),
            None => rt.block_on(crate::commands::dispatch_async(&mut ctx, head, rest)),
        };
        if let Some(out) = handled {
            println!("{}", out.text);
            return Ok(());
        }
        // Dispatch answered NOTHING and the word still looks like a
        // command: the one surface with no unknown-command answer left.
        // The REPL refuses ("unknown command", chat.rs); a one-shot
        // that fell through here used to hand the typo to the provider
        // — a failed turn on a dead route, a BILLED one on a live one,
        // and a model answer to a word the user meant as a command.
        if let Some(refusal) = unknown_slash_refusal(head, rest) {
            println!("{refusal}");
            return Ok(());
        }
    }
    // A child's events go to THIS process's `session_tap`, which has no
    // subscribers here — the windows are all attached to the gateway,
    // in another process. `veles_core::relay` is the bridge, and it starts
    // only when the parent set `VELES_RELAY_TO`, so an ordinary
    // `veles chat -q` at a terminal does nothing at all.
    //
    // Inside `block_on` because the forwarder is a tokio task and needs
    // this runtime; flushed after the turn because `chat -q` exits the
    // moment the turn does, and the events worth having most are the
    // last ones.
    let relay = rt.block_on(async { veles_core::relay::start() });
    let tr = rt.block_on(boot.agent.run_turn(&mut history, query, None, printer));
    if let Some(r) = relay {
        rt.block_on(r.flush());
    }
    let tr = tr?;
    if quiet {
        // Machine-readable mode (delegate_task children): final reply only.
        println!("{}", tr.reply);
    }
    Ok(())
}

/// Event printer for quiet mode: nothing but silence (reply prints at end).
pub fn quiet_event(_ev: StreamEvent) {}

/// The unknown-slash refusal for the one-shot path.
///
/// NARROW ON PURPOSE: only a single leading token shaped like a
/// command word — `^/[a-z][a-z0-9_-]*$` with nothing after it — is
/// refused. `/home/user/report.md summarize this` is a PROMPT that
/// begins with a path, not a typo, and must still reach the model;
/// so must `/dev/null is fine`, and a word with arguments could be
/// a custom command's invocation shape (`/review src/`).
///
/// Called AFTER the dispatchers have answered nothing and the
/// custom/plugin/alias tables have missed, so a real command can
/// never hit it.
fn unknown_slash_refusal(head: &str, rest: &str) -> Option<String> {
    // Arguments disqualify: with `rest` the caller meant SOMETHING,
    // and the something may be a custom command's argument shape.
    // Refusing only the bare word keeps the guard on typos.
    if !rest.is_empty() {
        return None;
    }
    let word = head.strip_prefix('/')?;
    let looks_like_command = !word.is_empty()
        && word.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !looks_like_command {
        return None;
    }
    // One more chance to be real: disk custom command, plugin command,
    // config alias. `find_any` and `resolve_command` are the same
    // lookups the REPL's unknown-command path makes before refusing.
    if veles_core::commands::find_any(head).is_some() {
        return None;
    }
    let cfg = veles_core::config::live();
    if veles_core::aliases::resolve_command(&cfg, head).is_some() {
        return None;
    }
    // Near-miss: one edit away, computed the honest way — a tiny
    // bounded Levenshtein over the builtin table's names (they number
    // in the dozens, so "tiny" is fine here). /crns → /crons is the
    // case that shipped without it: "crons".contains("crns") is false,
    // the transpose is a real edit.
    fn within_one_edit(a: &str, b: &str) -> bool {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        if a.len().abs_diff(b.len()) > 1 {
            return false;
        }
        // Classic two-row DP, abandoned the moment a row exceeds 1.
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0usize; b.len() + 1];
        for (i, ca) in a.iter().enumerate() {
            cur[0] = i + 1;
            let mut row_min = cur[0];
            for (j, cb) in b.iter().enumerate() {
                cur[j + 1] = (prev[j] + usize::from(ca != cb))
                    .min(prev[j + 1] + 1)
                    .min(cur[j] + 1);
                row_min = row_min.min(cur[j + 1]);
            }
            if row_min > 1 {
                return false;
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()] <= 1
    }
    let mut near: Vec<&str> = Vec::new();
    for (name, _) in crate::commands::help_entries(crate::commands::Surface::Repl) {
        let n = name.split(' ').next().unwrap_or(name);
        let bare = n.trim_start_matches('/');
        if bare == word {
            continue; // a builtin would have answered already
        }
        if within_one_edit(bare, word) || bare.contains(word) || word.contains(bare) {
            near.push(n);
        }
    }
    near.dedup();
    let hint = if near.is_empty() {
        String::new()
    } else {
        format!(" — did you mean {}?", near.join(", "))
    };
    Some(format!(
        "unknown command: {head} (try /help{hint}) — not sent to the model"
    ))
}

pub enum PluginsCmd {
    List,
    Rollback(String),
    Hooks,
    /// name, description
    New(String, String, String),
    /// url, optional name to install under
    Install(String, Option<String>),
    Remove(String),
    /// name, the built module file
    InstallModule(String, std::path::PathBuf),
}

pub fn plugins(action: Option<PluginsCmd>) -> Result<()> {
    let dir = veles_core::plugins::plugins_dir();
    match action {
        // Built on discover_ALL, with a state column — not on the
        // filtered `discover`. The desktop's plugin overlay learned
        // this the hard way (PENDING, 2026-08-30): a list built on the
        // filtered call has no row for a plugin you switched off, so it
        // vanishes from the UI entirely and there is nothing left to
        // click to switch it back on. The CLI had the identical bug and
        // it was invisible for a different reason — the directory was
        // wrong too, so the list was empty either way.
        None | Some(PluginsCmd::List) => {
            let cfg = veles_core::config::load().unwrap_or_default();
            let found = veles_core::plugins::discover_all()?;
            // The directory is named on SUCCESS too, not only when the
            // list is empty. It named the wrong one for a day and the
            // only surface that said so was the failure message nobody
            // had a reason to read.
            println!("{}", dir.display());
            // The bundled tier's roots that exist: a plugin listed
            // below with src=bundled came from one of these, read-only.
            for root in veles_core::plugins::bundled_roots() {
                if root.is_dir() {
                    println!("{} (bundled, read-only)", root.display());
                }
            }
            if found.is_empty() {
                println!("(no directory here has a plugin.toml)");
                return Ok(());
            }
            let (mut on, mut off) = (0usize, 0usize);
            for p in &found {
                let live = veles_core::plugins::is_enabled(
                    &p.manifest.name,
                    &cfg.plugins.enabled,
                    &cfg.plugins.disabled,
                );
                if live {
                    on += 1;
                } else {
                    off += 1;
                }
                println!(
                    "{} {:<24} kind={:<5} src={:<7} hooks={}",
                    if live { "\u{25cf}" } else { "\u{25cb}" },
                    p.manifest.name,
                    p.manifest.kind,
                    if veles_core::plugins::is_bundled(p) {
                        "bundled"
                    } else {
                        "user"
                    },
                    if p.manifest.provides_hooks.is_empty() {
                        "-".to_string()
                    } else {
                        p.manifest.provides_hooks.join(",")
                    }
                );
            }
            println!("\n\u{25cf} {on} loaded   \u{25cb} {off} present but not loaded");
            // A non-empty `enabled` is an allowlist, which is the one
            // config shape where a plugin is off without being named in
            // `disabled` — worth saying, because otherwise a ring
            // beside a plugin nobody disabled reads as a bug.
            if !cfg.plugins.enabled.is_empty() {
                println!(
                    "[plugins] enabled is an ALLOWLIST ({}), so anything absent from it is off",
                    cfg.plugins.enabled.join(", ")
                );
                let missing: Vec<&String> = cfg
                    .plugins
                    .enabled
                    .iter()
                    .filter(|e| !found.iter().any(|p| &p.manifest.name == *e))
                    .collect();
                if !missing.is_empty() {
                    println!(
                        "  allowlisted but not installed: {}",
                        missing
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            Ok(())
        }
        // Delegates to plugins::rollback rather than shelling out to
        // git here, which is what this branch used to do. Two reasons
        // it mattered: that copy read the wrong directory (see
        // plugins::discover_all), and — worse — it skipped the
        // snapshot-before-checkout that plugins::rollback does, so the
        // state you were rolling back FROM was unrecoverable. The undo
        // button was a one-way door, which is the exact thing that
        // function's docstring says it must not be.
        Some(PluginsCmd::Rollback(commit)) => {
            veles_core::plugins::rollback(&commit)?;
            println!("plugin state rolled back to {commit}");
            println!("(the state before this rollback was snapshotted first)");
            println!("(running agents pick it up via hot reload; restart to be safe)");
            Ok(())
        }
        // create / install / remove.
        //
        // `plugins::scaffold`, `install` and `remove` have existed and
        // been reachable from exactly ONE of five frontends — the
        // desktop's plugin manager, over its HTTP API. Somebody working
        // from a terminal, which is where plugin authors work, had no
        // way to make a plugin at all: `veles plugins` could list, roll
        // back and print the hook taxonomy. These are three-line
        // wrappers over functions that were already there and already
        // tested; the gap was wiring, not capability.
        Some(PluginsCmd::New(name, description, kind)) if kind == "overlay" => {
            let here = std::env::current_dir()?;
            let created = veles_core::plugins::scaffold_overlay(&name, &description, &here)?;
            println!("{}", created.display());
            println!("  Cargo.toml    the crate: a dylib on veles-runtime");
            println!("  src/lib.rs    the module, with one tool you can edit");
            println!("  flake.nix     builds veles with this module in it");
            println!("  README.md     build, install, update");
            println!("\nSet `inputs.veles.url` in flake.nix to the veles you run, `git init` and");
            println!("`git add` the files (a flake sees only tracked files), then `nix build`.");
            println!("Not part of the stable extension API: docs/extending.md,");
            println!("\"Rust overlay modules\".");
            Ok(())
        }
        Some(PluginsCmd::New(name, description, kind)) => {
            let created = veles_core::plugins::scaffold_kind(&name, &description, &kind)?;
            println!("{}", created.display());
            println!("  plugin.toml   manifest (name, kind, entry, provides_hooks)");
            if kind == "js" {
                println!("  plugin.ts     a tool and a post_llm_call handler you can edit");
                println!(
                    "\nIt is live on the next reload — `veles plugins reload {name}`, or just"
                );
                println!("save the file: the runtime watches this directory.");
                println!("API reference: docs/plugins.md, \"JavaScript / TypeScript\"");
            } else {
                println!("  plugin.tcl    a post_llm_call handler you can edit");
                println!("\nIt is live on the next reload — `veles tcl reload`, or just");
                println!("save the file: the runtime watches this directory.");
                println!("API reference: {}", dir.join("veles_api.tcl").display());
            }
            Ok(())
        }
        Some(PluginsCmd::Install(url, as_name)) => {
            let dest = veles_core::plugins::install(&url, as_name.as_deref())?;
            println!("installed {}", dest.display());
            println!("(enable it in [plugins] if `enabled` is a non-empty allowlist)");
            Ok(())
        }
        // A built module into `<data>/modules/<name>/`: the content-
        // addressed file, `current`, plugin.toml — what
        // scripts/install-module-dev.sh and push-plugin.sh do with
        // `install`/`ln`, done by the engine so it is the same on every
        // platform (Windows has no symlinks to speak of; the pointer
        // file is `current` there).
        Some(PluginsCmd::InstallModule(name, file)) => {
            anyhow::ensure!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "a module name is letters, digits, - and _: {name:?}"
            );
            anyhow::ensure!(file.is_file(), "{}: not a file", file.display());
            let dir = veles_core::config::data_dir().join("modules").join(&name);
            let so = veles_core::modulehost::install_file(&dir, &file)?;
            let manifest = format!(
                "name = \"{name}\"\nkind = \"module\"\nentry = \"{}\"\n",
                veles_core::modulehost::DEFAULT_ENTRY
            );
            std::fs::write(dir.join("plugin.toml"), manifest)?;
            println!("{name} → {} (current)", so.display());
            println!("(the gateway loads it at its next start, or `veles plugins rescan`)");
            Ok(())
        }
        Some(PluginsCmd::Remove(name)) => {
            let gone = veles_core::plugins::remove(&name)?;
            println!("removed {}", gone.display());
            println!("(the state before this was snapshotted — `veles plugins rollback`)");
            Ok(())
        }
        // The taxonomy was 27 names in one flat list, so a plugin author
        // could register for a hook nothing emits and wait forever for
        // a call. This says which is which, and — by booting the Tcl
        // runtime — which have a handler attached right now.
        Some(PluginsCmd::Hooks) => {
            let registered: std::collections::HashSet<String> =
                match tclrt::spawn(veles_core::config::config_dir()) {
                    Ok(h) => h
                        .eval("lsort [array names ::veles::hooks]")
                        .map(|s| s.split_whitespace().map(str::to_string).collect())
                        .unwrap_or_default(),
                    Err(e) => {
                        println!("(Tcl runtime unavailable: {e} — handler column omitted)");
                        Default::default()
                    }
                };
            let mark = |h: &str| {
                if registered.contains(h) {
                    "  ← handler registered"
                } else {
                    ""
                }
            };
            println!("FIRED — the engine emits these:");
            for h in veles_core::hooks::FIRED_HOOKS {
                println!("  {h:<28}{}", mark(h));
            }
            println!("\nRESERVED — accepted for hermes plugin portability, never emitted:");
            for h in veles_core::hooks::RESERVED_HOOKS {
                println!("  {h:<28}{}", mark(h));
            }
            println!(
                "\nRegistering for a reserved hook is legal and does nothing. \
                 A fired hook with no handler costs nothing either — \
                 HookBus::fire short-circuits before the thread hop."
            );
            Ok(())
        }
    }
}

/// Ctrl-C ends the TURN, not the process.
///
/// `chat -q` is what `delegate_task` spawns children with, and both of
/// its turn sites passed `None` for the cancel token — so the only way
/// to stop one was to kill the process, losing the transcript mid-write
/// and leaving the store's last row half-finished. `run_turn` now
/// publishes every turn's token (`agent::cancel_turn`), so this only has
/// to find the signal.
///
/// A SECOND Ctrl-C exits. Installing a tokio signal handler replaces the
/// default disposition, so without this the process would become
/// un-interruptible — a worse bug than the one being fixed.
fn spawn_interrupt_handler(rt: &tokio::runtime::Runtime, session_id: &str) {
    let sid = session_id.to_string();
    rt.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n(cancelling the turn — Ctrl-C again to quit)");
            veles_core::agent::cancel_turn(&sid);
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
}

pub enum CronCmd {
    List,
    /// `session`: optional wakeup target (Stage 5) — resume THAT
    /// session instead of a fresh one.
    Add {
        schedule: String,
        prompt: String,
        session: Option<String>,
    },
    Run(String),
    Pause(String),
    Resume(String),
    Remove(String),
    History(String),
}

/// `veles triggers …` — the trigger table, same store-only shape as
/// cron. `refresh`/`mask` talk to slopdrop over HTTP, so they run the
/// async command half under a local runtime; everything else is the
/// sync shared-command body.
pub enum TriggersCmd {
    List,
    /// "<name> k=v…" — one string, parsed by the shared `triggers_add`.
    Add {
        name: String,
        spec: Vec<String>,
    },
    Show(String),
    Enable(String),
    Disable(String),
    Rm(String),
    Log(Option<String>),
    Test {
        name: String,
        text: String,
    },
    Mask {
        network: String,
        channel: String,
        events: String,
    },
    Refresh,
}

pub enum TasksCmd {
    List,
    Output(u32),
    Cancel(u32),
}

/// `veles tasks …` — the running-work surface.
///
/// The task registry is in-memory and process-global, so a fresh CLI
/// process has no tasks of its own by construction: there is nothing to
/// list locally and pretending otherwise would print an empty table that
/// looks like an answer. This asks the RUNNING instance over loopback,
/// the same way `open_preview` does, and says plainly when none is up.
pub fn tasks(action: Option<TasksCmd>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        match action.unwrap_or(TasksCmd::List) {
            TasksCmd::List => {
                let v = api_get("/api/tasks").await?;
                let listing = v["listing"].as_str().unwrap_or("");
                if listing.is_empty() {
                    println!("{}", veles_core::i18n::lookup("tasks_none", &[]));
                } else {
                    print!("{listing}");
                }
            }
            TasksCmd::Output(id) => {
                let v = api_get(&format!("/api/tasks/{id}/output")).await?;
                if let Some(e) = v["error"].as_str() {
                    println!(
                        "{}",
                        veles_core::i18n::lookup("tasks_not_found", &[("id", id.to_string())])
                    );
                    tracing::debug!("tasks output: {e}");
                    return Ok(());
                }
                println!("{}", v["output"].as_str().unwrap_or(""));
            }
            TasksCmd::Cancel(id) => {
                let v = api_post(&format!("/api/tasks/{id}/cancel")).await?;
                match v["cancelled"].as_u64() {
                    Some(_) => println!(
                        "{}",
                        veles_core::i18n::lookup("tasks_cancelled", &[("id", id.to_string())])
                    ),
                    None => println!(
                        "{}",
                        veles_core::i18n::lookup("tasks_not_running", &[("id", id.to_string())])
                    ),
                }
            }
        }
        Ok(())
    })
}

fn instance_port() -> u16 {
    std::env::var("VELES_DESKTOP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8642)
}

const NO_INSTANCE: &str =
    "no running veles instance on loopback (start one with `veles gui` or `veles gateway run`)";

async fn api_get(path: &str) -> Result<serde_json::Value> {
    let url = format!("http://127.0.0.1:{}{path}", instance_port());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!(NO_INSTANCE))?;
    Ok(resp.json::<serde_json::Value>().await?)
}

async fn api_post(path: &str) -> Result<serde_json::Value> {
    let url = format!("http://127.0.0.1:{}{path}", instance_port());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(&url)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!(NO_INSTANCE))?;
    Ok(resp.json::<serde_json::Value>().await?)
}

pub enum CheckpointCmd {
    Status,
    List(Option<String>),
    Show(i64),
    Restore(i64),
    Prune,
}

/// `veles checkpoints …` — the undo surface for the agent's file edits.
pub fn checkpoints(action: Option<CheckpointCmd>) -> Result<()> {
    let home = veles_core::config::veles_home();
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    match action.unwrap_or(CheckpointCmd::Status) {
        CheckpointCmd::Status => {
            let (n, bytes) = store.lock_ok().checkpoint_stats()?;
            println!(
                "{n} checkpoint(s), {:.1} MB under {}",
                bytes as f64 / (1024.0 * 1024.0),
                veles_core::checkpoints::blob_root().display()
            );
            let recent = store.lock_ok().checkpoint_list(None, 5)?;
            if !recent.is_empty() {
                println!("{}", veles_core::checkpoints::render_list(&recent));
            }
        }
        CheckpointCmd::List(session) => {
            let items = store
                .lock()
                .unwrap()
                .checkpoint_list(session.as_deref(), 50)?;
            println!("{}", veles_core::checkpoints::render_list(&items));
        }
        CheckpointCmd::Show(id) => {
            let cp = store
                .lock()
                .unwrap()
                .checkpoint_get(id)?
                .ok_or_else(|| anyhow::anyhow!("no checkpoint {id}"))?;
            println!(
                "#{} {} · {} · {} bytes · session {}\npath: {}",
                cp.id, cp.tool, cp.created_at, cp.bytes, cp.session_id, cp.path
            );
            match &cp.blob {
                Some(b) => {
                    println!("blob: {b}");
                    // A preview is more useful than the digest here: the
                    // question being answered is "is this the version I
                    // want back?".
                    if let Ok(text) = std::fs::read_to_string(b) {
                        let head: String = text.lines().take(20).collect::<Vec<_>>().join("\n");
                        println!("--- first 20 lines ---\n{head}");
                    }
                }
                None => println!("(file did not exist — restoring deletes it)"),
            }
        }
        CheckpointCmd::Restore(id) => {
            println!("{}", veles_core::checkpoints::restore(&store, id)?);
        }
        CheckpointCmd::Prune => {
            let (n, freed) = veles_core::checkpoints::prune(&store)?;
            println!(
                "pruned {n} checkpoint(s), freed {:.1} MB",
                freed as f64 / (1024.0 * 1024.0)
            );
        }
    }
    Ok(())
}

/// `veles toolstats` — the tool-usage regression watch.
///
/// Reads the persisted tool rows straight from the store (no gateway
/// needed; this is a report over history, not live state). Default: total
/// calls per tool, all time, most-used first. `--days N` narrows the
/// window; `--tool NAME` breaks that one tool out by day, where ABSENT
/// days are the point — a tool that stops appearing has gone dark. This
/// is the facility that caught delegate_task at 8 calls and shell/monitor
/// at 0 while terminal ran 25k times, which is the evidence behind
/// promoting those three to the core tier. Without a way to SEE the
/// counts, the next "token efficiency" demotion looks free until somebody
/// notices the capability vanished.
pub fn toolstats(days: Option<i64>, tool: Option<String>, limit: usize) -> Result<()> {
    let home = veles_core::config::veles_home();
    let store = Store::open(&home.join("state.db"))?;
    if let Some(name) = tool {
        let rows = store.tool_usage_by_day(&name, days)?;
        if rows.is_empty() {
            println!(
                "`{name}`: no calls{}",
                days.map(|d| format!(" in the last {d} days"))
                    .unwrap_or_default()
            );
            return Ok(());
        }
        let total: i64 = rows.iter().map(|(_, c)| c).sum();
        let window = days
            .map(|d| format!(" (last {d} days)"))
            .unwrap_or_else(|| " (all time)".into());
        println!(
            "`{name}`{window}: {total} call(s) over {} active day(s)",
            rows.len()
        );
        for (d, c) in rows {
            println!("  {d}  {c}");
        }
        return Ok(());
    }
    let rows = store.tool_usage(days)?;
    if rows.is_empty() {
        println!("no tool calls recorded");
        return Ok(());
    }
    let window = days
        .map(|d| format!(" in the last {d} days"))
        .unwrap_or_else(|| " (all time)".into());
    let total: i64 = rows.iter().map(|(_, c)| c).sum();
    println!(
        "tool usage{window}: {total} call(s) across {} tool(s)",
        rows.len()
    );
    let width = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, c) in rows.into_iter().take(limit.max(1)) {
        println!("  {name:<width$}  {c}");
    }
    Ok(())
}

pub fn cron(action: Option<CronCmd>) -> Result<()> {
    let home = veles_core::config::veles_home();
    let store = Store::open(&home.join("state.db"))?;
    match action.unwrap_or(CronCmd::List) {
        CronCmd::List => {
            // One formatter, four surfaces — and the CLI gains the
            // scheduler-liveness line it never had, which is the whole
            // point of the shared version.
            println!(
                "{}",
                veles_core::cron::listing(&std::sync::Arc::new(std::sync::Mutex::new(store)))
            );
            return Ok(());
        }
        CronCmd::Add {
            schedule,
            prompt,
            session,
        } => {
            if prompt.trim().is_empty() {
                anyhow::bail!(
                    "a job needs a prompt: veles cron add <schedule> [--session <id>] <prompt>"
                );
            }
            let summary =
                veles_core::cron::add_local_job(&store, &schedule, &prompt, session.as_deref())?;
            println!("{summary}");
            match veles_core::lifecycle::read_pid()
                .filter(|p| veles_core::lifecycle::is_running(*p))
            {
                Some(pid) => println!("the scheduler is running (gateway pid {pid})"),
                None => println!(
                    "NOTE: no scheduler is running, so this will not fire until a gateway is up"
                ),
            }
        }
        CronCmd::Run(id) => {
            let Some(job) = store.cron_get(&id)? else {
                anyhow::bail!("no job with id '{id}'");
            };
            let store = std::sync::Arc::new(std::sync::Mutex::new(store));
            let cfg = std::sync::Arc::new(veles_core::config::load()?);
            // `deliver = "home"` resolves here as in the gateway (the ops
            // chat, veles_core::home), so the run detail names the same chat.
            veles_core::home::install(&cfg);
            let rt = tokio::runtime::Runtime::new()?;
            let summary = rt.block_on(async {
                let sched = veles_core::cron::Scheduler::new(
                    store,
                    cfg,
                    veles_core::hooks::HookBus::new(None),
                    None,
                );
                sched.run_job_manual(&job).await
            });
            println!("{summary}");
        }
        CronCmd::Pause(id) => {
            if store.cron_set_enabled(&id, false)? {
                println!("job {id} paused");
            } else {
                anyhow::bail!("no job with id '{id}'");
            }
        }
        CronCmd::Resume(id) => {
            if store.cron_set_enabled(&id, true)? {
                println!("job {id} resumed");
            } else {
                anyhow::bail!("no job with id '{id}'");
            }
        }
        CronCmd::Remove(id) => {
            if store.cron_delete(&id)? {
                println!("job {id} removed");
            } else {
                anyhow::bail!("no job with id '{id}'");
            }
        }
        CronCmd::History(id) => {
            let runs = store.cron_run_history(&id, 10)?;
            if runs.is_empty() {
                println!("no runs recorded for {id}");
            }
            for r in runs {
                println!(
                    "{:<8} {} → {} {}",
                    r.status,
                    r.started_at,
                    r.finished_at,
                    r.detail.unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}

/// `veles triggers …` — the shared command answers, store-first.
///
/// The `AclCtx` is built over a loaded config's `[acl]` and the store
/// Arc — the same seam the gateway's dispatcher passes, so the CLI's
/// answer is the shared implementation's, not a reimplementation.
/// `refresh` and `mask` await slopdrop over HTTP under a local
/// runtime; everything else stays sync.
pub fn triggers(action: Option<TriggersCmd>) -> Result<()> {
    let cfg = load()?;
    let home = veles_core::config::veles_home();
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    let ctx = super::commands::AclCtx {
        store: &store,
        fallback: &cfg.acl,
        session_id: "",
        principal: None,
        sender: None,
    };
    let rest = match action.unwrap_or(TriggersCmd::List) {
        TriggersCmd::List => String::new(),
        TriggersCmd::Add { name, spec } => {
            let mut line = format!("add {name}");
            if !spec.is_empty() {
                line.push(' ');
                line.push_str(&spec.join(" "));
            }
            line
        }
        TriggersCmd::Show(name) => format!("show {name}"),
        TriggersCmd::Enable(name) => format!("enable {name}"),
        TriggersCmd::Disable(name) => format!("disable {name}"),
        TriggersCmd::Rm(name) => format!("rm {name}"),
        TriggersCmd::Log(name) => match name {
            Some(n) => format!("log {n}"),
            None => "log".to_string(),
        },
        TriggersCmd::Test { name, text } => format!("test {name} {text}"),
        TriggersCmd::Mask {
            network,
            channel,
            events,
        } => {
            format!("mask {network} {channel} {events}")
        }
        TriggersCmd::Refresh => "refresh".to_string(),
    };
    if rest == "refresh" {
        let rt = tokio::runtime::Runtime::new()?;
        let ans = rt.block_on(super::commands::triggers_command_async(&ctx, &cfg, &rest));
        println!("{ans}");
        return Ok(());
    }
    let ans = super::commands::triggers_command(&ctx, &rest);
    println!("{ans}");
    Ok(())
}

/// Every automatic mechanism, whether it runs, and how to change it.
///
/// The audit that produced this found two separate ways a mechanism can
/// be invisible: WIRED-but-undocumented (the hook taxonomy listed 27
/// names, 12 of which fire) and PRESENT-but-unwired (most of guards.rs
/// is reached only by its own tests). Both look identical from the
/// outside — a green suite and a name in the source — so the only
/// honest fix is to print which is which.
fn doctor_mechanisms() -> Result<()> {
    let cfg = load().unwrap_or_default();
    let on = |b: bool| if b { "ON " } else { "off" };

    println!("mechanisms — what runs automatically\n");

    println!("  GUARDS (wired into the turn loop)");
    println!(
        "    {} tool-repeat guardrail        warn at {}, halt at {}   [agent.guards] guardrail_warn_at / guardrail_halt_at",
        on(true),
        cfg.agent.guards.guardrail_warn_at,
        cfg.agent.guards.guardrail_halt_at
    );
    println!(
        "    {} iteration budget             cap {}              [agent.guards] max_iterations",
        on(true),
        cfg.agent.guards.max_iterations
    );
    println!(
        "    {} verification nudge           max {}/turn            [agent.guards] verification_nudge",
        on(cfg.agent.guards.verification_nudge),
        cfg.agent.guards.verification_max_attempts
    );
    println!(
        "        └─ injects a synthetic USER message and costs a full extra\n\
         \x20          round each time it fires. Default OFF; set true to restore."
    );
    println!(
        "    {} verification hint            on the edit's result   [agent.guards] verification_hint",
        on(cfg.agent.guards.verification_hint),
    );
    println!(
        "        └─ the cheap replacement: one line appended to write_file/patch\n\
         \x20          output — no extra round, no words in the user's mouth — plus a\n\
         \x20          zero-token note to the human when a turn ends unverified."
    );

    println!("\n  PROMPT RESIDENCY (what every round pays for)");
    println!(
        "    {} tool tiering                 tier = {:?}         [tools] tier",
        on(cfg.tools.tier != "full"),
        cfg.tools.tier
    );
    println!(
        "    {} skills index mode            mode = {:?}, max {}   [skills] index_mode",
        on(cfg.skills.index_mode != "full"),
        cfg.skills.index_mode,
        cfg.skills.index_max_entries
    );
    println!(
        "    {} memory resident caps         core {}, user {}  [memory] core_char_limit / user_resident_char_limit",
        on(cfg.memory.core_char_limit > 0),
        cfg.memory.core_char_limit,
        cfg.memory.user_resident_char_limit
    );
    println!(
        "    {} tool output cap              {} chars          [agent] tool_output_cap",
        on(cfg.agent.tool_output_cap > 0),
        cfg.agent.tool_output_cap
    );

    println!("\n  WIRE");
    println!(
        "    {} prompt cache accounting      mode = {:?}        [cache] enabled / mode",
        on(cfg.cache.enabled),
        cfg.cache.mode
    );
    println!(
        "    {} sticky failover in a turn                        [agent.failover] sticky_within_turn",
        on(cfg.agent.failover.sticky_within_turn)
    );
    println!(
        "    {} mid-turn compaction          at {}% of window   [agent.compaction] mid_turn_percent",
        on(cfg.agent.compaction.mid_turn_percent > 0.0),
        cfg.agent.compaction.mid_turn_percent
    );

    println!("\n  HOOKS");
    println!(
        "    {} fired by the engine, {} reserved for hermes compat (never emitted)",
        veles_core::hooks::FIRED_HOOKS.len(),
        veles_core::hooks::RESERVED_HOOKS.len()
    );
    println!("        `veles plugins hooks` lists both, and which have handlers.");

    println!("\n  PRESENT BUT NOT WIRED — no config turns these on; they are");
    println!("  code nothing calls. Listed so nobody mistakes them for active:");
    for g in veles_core::guards::UNWIRED_GUARDS {
        println!("    ·  guards::{g}");
    }
    println!(
        "\n  Failover has ONE source of truth: ProviderError::should_fallback\n\
         \x20 decides whether an error is worth moving for, and failover.rs\n\
         \x20 decides the policy (ask | auto | stop | script, [[failover]] rules,\n\
         \x20 vetoable per-hop hooks). The duplicate taxonomy that used to live\n\
         \x20 in guards.rs was deleted on 2026-09-02."
    );
    Ok(())
}

/// `veles doctor --sandbox` — resolve the configured fence and PROVE it.
///
/// Every other doctor check reads something. This one runs something:
/// it resolves `[sandbox]` through the same `backend_from` the engine
/// uses, reports the binary and each artifact that backend needs, then
/// executes `puts [expr {6*7}]` through it and checks for `42`.
///
/// It exists because nothing else answered "does the fence work here?".
/// Five backends shipped 2026-09-03 with argv-pinning tests and an
/// opt-in VM script that hand-rolled equivalent commands — so a wrong
/// `NspawnBackend` could not have failed anything. This is also the
/// single command every install script (Debian, Ubuntu, Arch, Gentoo,
/// FreeBSD, the NixOS module) calls to prove its own install instead of
/// printing "done".
/// The node this node's user boxes are made on, when it is ANOTHER one:
/// `[cluster] sandbox_home` naming a node that is not this one. `None`
/// when boxes are made here — unset, blank, `local`/`here`, or our own
/// id or name (`remotetool::is_here`, the same reading the tools use).
fn boxes_elsewhere(sandbox_home: Option<&str>) -> Option<String> {
    let node = sandbox_home.map(str::trim).filter(|n| !n.is_empty())?;
    (!veles_core::remotetool::is_here(node)).then(|| node.to_string())
}

pub fn doctor_sandbox() -> Result<()> {
    let section = veles_core::config::SandboxSection::current();
    println!("veles doctor --sandbox");
    // `[sandbox] backend` may be `auto` (the phone's default): the
    // backend it comes to, and why, is what every line below is about.
    let effective = veles_core::sandbox::effective_backend(&section);
    if section.backend.trim() == "auto" {
        println!("  backend: auto → {effective}");
    } else {
        println!("  backend: {effective}");
    }
    // Everything below reads the resolved name, not the key.
    let section = veles_core::config::SandboxSection {
        backend: effective.name.clone(),
        ..section
    };

    // What this backend needs on this host, resolved BEFORE trying to
    // run: an absent binary and an absent artifact are different
    // failures and deserve different messages.
    /// (label, resolved value) — the binary a backend needs, and each
    /// artifact it needs, in the words the config uses for them.
    type Requirement = (&'static str, String);
    let (binary, artifacts): (Option<Requirement>, Vec<Requirement>) = match section.backend.trim()
    {
        "native" | "host" => (
            Some(("tclsh", veles_core::executecode::tclsh_path())),
            Vec::new(),
        ),
        "podman" => (
            Some((
                "podman",
                std::env::var("VELES_PODMAN_BIN").unwrap_or_else(|_| "podman".into()),
            )),
            // The image is a podman-local name, not a path — podman
            // is the only thing that can answer whether it exists,
            // and `--pull=never` makes a missing one fail loudly at
            // run time. Named here, not stat'd.
            Vec::new(),
        ),
        "nspawn" => (
            Some((
                "systemd-nspawn",
                std::env::var("VELES_NSPAWN_BIN").unwrap_or_else(|_| "systemd-nspawn".into()),
            )),
            vec![("rootfs ([sandbox] image)", section.image.clone())],
        ),
        "firecracker" => (
            Some((
                "firecracker",
                std::env::var("VELES_FIRECRACKER_BIN").unwrap_or_else(|_| "firecracker".into()),
            )),
            vec![
                (
                    "kernel ([sandbox] kernel_path)",
                    section.kernel_path.clone().unwrap_or_default(),
                ),
                ("rootfs ([sandbox] image)", section.image.clone()),
                ("KVM device", "/dev/kvm".to_string()),
            ],
        ),
        "jail" => (
            Some((
                "jail",
                std::env::var("VELES_JAIL_BIN").unwrap_or_else(|_| "jail".into()),
            )),
            vec![("jail root ([sandbox] image)", section.image.clone())],
        ),
        // The phone's userland package: the engine's shell must be its
        // ulsh, which the gateway sets only once a probe ran there.
        "userland" => (
            Some((
                "ulsh (the userland's shell)",
                veles_core::userland::ulsh()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "libulsh.so as VELES_SHELL".into()),
            )),
            Vec::new(),
        ),
        _ => (None, Vec::new()),
    };

    let mut failed: Vec<String> = Vec::new();
    if let Some((label, bin)) = &binary {
        match veles_core::shell::which(bin) {
            Some(p) => println!("  {label}: {}", p.display()),
            None => {
                println!("  {label}: NOT FOUND ({bin} is not on PATH)");
                failed.push(format!("{label} missing"));
            }
        }
    }
    for (label, path) in &artifacts {
        if path.trim().is_empty() {
            println!("  {label}: NOT CONFIGURED");
            failed.push(format!("{label} not configured"));
        } else if std::path::Path::new(path).exists() {
            println!("  {label}: {path}");
        } else {
            println!("  {label}: MISSING ({path})");
            failed.push(format!("{label} missing"));
        }
    }
    if section.backend == "podman" {
        println!("  image ([sandbox] image): {}", section.image);
    }
    println!(
        "  policy: network={}, capabilities={}, memory_mb={}, mounts={}",
        section.network,
        section.capabilities,
        section.memory_mb,
        section.mounts.len()
    );

    // The user-env fence's outbound, which no other readout answers.
    //
    // `[sandbox] user_env_network = "isolated"` (the default) passes no
    // `--network` flag and trusts podman to build a namespace that has
    // outbound in it. On a rootless podman that outbound comes from pasta
    // or slirp4netns, and a host with neither still creates the box:
    // nothing errors, the box just cannot reach anything, and
    // `net_mode_matches` reports the configured posture as satisfied.
    // `ensure` warns once and carries on — a helper-less host must not
    // fence everyone out of a working box — so this is where the finding
    // is allowed to be a problem.
    let rt = tokio::runtime::Runtime::new()?;
    if section.backend == "podman" {
        let user_net = veles_core::user_envs::EnvCfg::current().net;
        let state = rt.block_on(veles_core::user_envs::rootless_net_state());
        match user_net {
            veles_core::user_envs::EnvNet::Isolated => {
                println!("  user envs: network=isolated, {}", state.describe());
                if !state.isolated_has_outbound() {
                    println!(
                        "             boxes will have NO outbound and nothing can be published \
                         into them;\n             install pasta or slirp4netns, or set \
                         [sandbox] user_env_network = \"none\""
                    );
                    failed.push(
                        "isolated user envs have no outbound (no rootless network driver)"
                            .to_string(),
                    );
                }
            }
            other => println!(
                "  user envs: network={} — the rootless driver does not apply",
                other.spelling()
            ),
        }
    }

    // Resolution can fail on its own (unknown name, firecracker without
    // a kernel line, a bad mount). Report that as the finding it is.
    match veles_core::sandbox::backend_from(&section) {
        Ok(b) => println!("  resolved: {}", b.name()),
        Err(e) => {
            println!("  resolved: FAILED ({e:#})");
            return Err(anyhow::anyhow!("doctor --sandbox: {e:#}"));
        }
    }

    // The part that is not a readout. A backend that resolves, has its
    // binary and has its artifacts can still fail to run — that is the
    // whole reason this executes rather than reports.
    println!("  running `puts [expr {{6*7}}]` through the fence…");
    let dir = std::env::temp_dir().join(format!("veles-doctor-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("probe.tcl");
    std::fs::write(&script, "puts [expr {6*7}]\n")?;
    let out = rt.block_on(veles_core::sandbox::execute(&script, 120));
    let _ = std::fs::remove_dir_all(&dir);

    match out {
        Ok(o) => {
            if !o.stderr.trim().is_empty() {
                println!("  stderr: {}", o.stderr.trim());
            }
            let got = o.stdout.trim().to_string();
            if o.success() && got.contains("42") {
                println!("  result: OK (42)");
            } else if o.timed_out {
                println!("  result: TIMED OUT");
                failed.push("run timed out".into());
            } else {
                println!("  result: FAILED (exit {}, stdout {got:?})", o.exit_code);
                failed.push(format!("run failed (exit {})", o.exit_code));
            }
        }
        Err(e) => {
            println!("  result: FAILED ({e:#})");
            failed.push(format!("{e:#}"));
        }
    }

    // User envs — the box every below-Admin caller's terminal, files and
    // execute_code land in — are podman whatever `backend` says, except
    // `backend = "userland"` (the phone's proot guests). A host
    // that cannot make one fails all of those calls, so make one — unless
    // this node's boxes are made on another node (`[cluster]
    // sandbox_home`: the phone, winbox). There "no podman here" is the
    // configured state, and the old verdict counted it as a problem whose
    // cure was the line the config already had (seen 2026-10-14).
    if let Some(node) = boxes_elsewhere(veles_core::config::live().cluster.sandbox_home.as_deref())
    {
        println!("  user envs: made on node {node} ([cluster] sandbox_home), none here");
    } else {
        println!(
            "  user envs: a box end to end through {}…",
            match veles_core::user_envs::runtime() {
                veles_core::user_envs::Runtime::PodmanRemote => "podman (remote: CONTAINER_HOST)",
                veles_core::user_envs::Runtime::Userland(_) =>
                    "the userland (a proot guest of its own)",
                _ => "podman",
            }
        );
        let userland = matches!(
            veles_core::user_envs::runtime(),
            veles_core::user_envs::Runtime::Userland(_)
        );
        match rt.block_on(veles_core::user_envs::doctor_round_trip()) {
            // A userland box is another app's storage: its /work is
            // emptied, the box itself stays for the next doctor.
            Ok(()) if userland => println!(
                "  user envs: OK (exec, a file in and out, the driver; /work emptied, the box kept)"
            ),
            Ok(()) => println!("  user envs: OK (exec, cp in and out, the driver, removed)"),
            Err(e) => {
                println!("  user envs: FAILED ({e:#})");
                failed.push(format!("user envs: {e:#}"));
            }
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "doctor --sandbox: {} problem(s): {}",
            failed.len(),
            failed.join("; ")
        ))
    }
}

/// `veles doctor --providers` — what wires this build speaks, which of
/// them a plugin supplied, and whether the configured route answers.
///
/// The same job `doctor --sandbox` does for the fence, and for the same
/// reason: a provider DRIVER is a capability a plugin claims, and a
/// claim nobody executed is not a capability. It also answers the
/// question a driver author actually has — "is my driver loaded, and
/// under what name" — which no other surface could say, because drivers
/// register at plugin load and config validation happens before that.
pub fn doctor_providers(probe: bool) -> Result<()> {
    println!(
        "veles doctor --providers{}",
        if probe { "" } else { " --no-probe" }
    );
    let mut failed: Vec<String> = Vec::new();

    // Booting the Tcl runtime is what LOADS plugins, which is what
    // registers drivers. Without it the driver list is empty and the
    // report would be a lie by omission.
    match tclrt::spawn(veles_core::config::config_dir()) {
        // WAIT for it. `spawn` returns as soon as the thread exists;
        // plugins load on that thread afterwards, so asking for the
        // driver list immediately gets an empty one — which this
        // command would then print as "no plugin has registered one",
        // on a machine where one had.
        Ok(h) => {
            if let Err(e) = h.wait_ready() {
                println!("  (tcl runtime did not come up: {e})");
            }
        }
        Err(e) => println!("  (tcl runtime unavailable: {e} — Tcl drivers not loaded)"),
    }

    let drivers = veles_core::providers::drivers();
    println!("\nregistered drivers ({}):", drivers.len());
    if drivers.is_empty() {
        println!("  (none — no plugin has called register_provider)");
    }
    for d in &drivers {
        println!("  {:<24} from {}", d.name, d.plugin);
    }

    let cfg = match veles_core::config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("\nconfig: UNREADABLE ({e:#})");
            return Err(anyhow::anyhow!("doctor --providers: config: {e:#}"));
        }
    };
    let routes = match veles_core::providers::build_routes(&cfg) {
        Ok(r) => r,
        Err(e) => {
            println!("\nroutes: FAILED ({e:#})");
            return Err(anyhow::anyhow!("doctor --providers: {e:#}"));
        }
    };

    println!("\nroute chain ({}):", routes.len());
    for r in &routes {
        let wire = match &r.client {
            veles_core::providers::RouteClient::ChatCompletions(_) => {
                "chat_completions".to_string()
            }
            veles_core::providers::RouteClient::Responses(_) => "responses".to_string(),
            veles_core::providers::RouteClient::Plugin(p) => {
                // A plugin route naming a driver nobody registered is
                // the failure this command exists to surface: config
                // load cannot check it, because plugins load later.
                match veles_core::providers::driver(&p.driver) {
                    Some(d) => format!("plugin:{} (from {})", p.driver, d.plugin),
                    None => {
                        failed.push(format!(
                            "route {} wants driver `{}`, which nothing registered",
                            r.provider, p.driver
                        ));
                        format!("plugin:{} — NOT REGISTERED", p.driver)
                    }
                }
            }
        };
        println!("  {:<28} {:<24} {wire}", r.provider, r.model);
    }

    // The part that is not a readout: a real turn.
    //
    // Down the CHAIN, not just the primary, because that is what the
    // engine does — `failover` moves off a throttled or dead route, so
    // a doctor that stops at the first 429 reports the fleet as broken
    // when it is working exactly as configured. Each route is named
    // with its own verdict; the command fails only if none answered.
    if probe && !routes.is_empty() {
        println!("\nasking each route for one word (stops at the first answer):");
        let rt = tokio::runtime::Runtime::new()?;
        let mut answered = false;
        let mut refusals: Vec<String> = Vec::new();
        for route in &routes {
            let req = veles_core::providers::ChatRequest {
                model: route.model.clone(),
                messages: vec![veles_core::providers::ChatMessage::user(
                    "Reply with exactly one word: ok",
                )],
                tools: vec![],
                reasoning_effort: None,
                stream: false,
                replay_reasoning: false,
                cache_mode: veles_core::providers::CacheMode::Off,
                extra_body: None,
            };
            match rt.block_on(route.client.chat(&req, &route.provider)) {
                Ok(c) if !c.message.content.trim().is_empty() => {
                    println!(
                        "  {:<28} OK ({:?})",
                        route.provider,
                        c.message.content.trim()
                    );
                    answered = true;
                    break;
                }
                Ok(_) => {
                    println!("  {:<28} empty answer", route.provider);
                    refusals.push(format!("{} answered nothing", route.provider));
                }
                Err(e) => {
                    // One line, not the provider's whole error body: a
                    // quota message can be a paragraph.
                    let short: String = e.to_string().chars().take(160).collect();
                    println!("  {:<28} {short}", route.provider);
                    refusals.push(format!("{}: {short}", route.provider));
                }
            }
        }
        if !answered {
            failed.push(format!(
                "no route answered ({} tried): {}",
                routes.len(),
                refusals.join("; ")
            ));
        }
    }

    if failed.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "doctor --providers: {} problem(s): {}",
            failed.len(),
            failed.join("; ")
        ))
    }
}

/// The doctor line for a leftover `~/.veles`, if one exists.
///
/// Nothing reads that directory any more. The pre-XDG layout was moved
/// into `~/.config/veles` and `~/.local/share/veles` from 2026-09-04,
/// and the migration that did it was removed on 2026-09-26 — so a
/// `~/.veles` that still exists holds files veles will never see, and
/// someone editing one would wait forever for it to take effect. Doctor
/// says so; it never looks inside.
pub(crate) fn legacy_home_note(user_home: &std::path::Path) -> Option<String> {
    let legacy = user_home.join(".veles");
    legacy.is_dir().then(|| {
        format!(
            "NOTE: {} exists — veles does not read it (config is {}, state is {}); \
             check what is in it and delete it",
            legacy.display(),
            veles_core::config::config_dir().display(),
            veles_core::config::data_dir().display()
        )
    })
}

pub fn doctor(
    prefix: bool,
    raw: bool,
    mechanisms: bool,
    sandbox: bool,
    providers: bool,
    no_probe: bool,
) -> Result<()> {
    if mechanisms {
        return doctor_mechanisms();
    }
    if sandbox {
        return doctor_sandbox();
    }
    if providers {
        return doctor_providers(!no_probe);
    }
    println!("veles doctor");
    println!("  version: {}", veles_core::VERSION);
    let home = veles_core::config::veles_home();
    // All THREE, since the 2026-09-04 XDG split. "home" was one
    // directory and one line; three directories reported as one is how
    // someone ends up editing a config.toml nothing reads.
    println!("  config: {}", veles_core::config::config_dir().display());
    println!("  state:  {}", home.display());
    println!("  cache:  {}", veles_core::config::cache_dir().display());
    if let Some(note) = directories::UserDirs::new().and_then(|d| legacy_home_note(d.home_dir())) {
        println!("  {note}");
    }
    // Doctor must not only PRINT failures — it must fail. A script
    // (or a human glancing at $?) gets exit 1 when any check did,
    // instead of the old green exit 0 over a FAILED route chain.
    let mut failed: Vec<&'static str> = Vec::new();
    match load() {
        Ok(cfg) => {
            println!(
                "  config file: OK (provider={:?}, model={:?}, custom_providers={}, fallbacks={})",
                cfg.model.provider,
                cfg.model.default,
                cfg.custom_providers.len(),
                cfg.fallback_providers.len()
            );
            match veles_core::providers::build_routes(&cfg) {
                Ok(routes) => {
                    println!("  route chain:");
                    for (i, r) in routes.iter().enumerate() {
                        // Per-route wire choices that cost money or can
                        // 400 an endpoint are named here, resolved —
                        // reading them back out of config.toml means
                        // re-deriving the defaults by hand.
                        let mut flags = Vec::new();
                        if r.replay_reasoning {
                            flags.push("replay_reasoning".to_string());
                        }
                        if let Some(e) = &r.reasoning_effort {
                            flags.push(format!("reasoning_effort={e}"));
                        }
                        if r.cache_mode != veles_core::providers::CacheMode::Auto {
                            flags.push(format!("cache={}", r.cache_mode.as_str()));
                        }
                        let suffix = if flags.is_empty() {
                            String::new()
                        } else {
                            format!("  ({})", flags.join(", "))
                        };
                        println!("    [{}] {} → {}{}", i, r.provider, r.model, suffix);
                    }
                    // A configured route that cannot authenticate is
                    // the state a fresh machine lands in: `veles
                    // migrate` brings the config and leaves the
                    // secrets, and until now the first turn was where
                    // you found out.
                    let missing = veles_core::providers::routes_missing_keys(&cfg, &routes);
                    if missing.is_empty() {
                        println!("  provider keys: OK ({} route(s))", routes.len());
                    } else {
                        for (provider, env) in &missing {
                            println!(
                                "  provider keys: MISSING for {provider} — set ${env} \
                                 (environment, or a line in {}/.env)",
                                veles_core::config::config_dir().display()
                            );
                        }
                        failed.push("provider keys");
                    }
                }
                Err(e) => {
                    println!("  route chain: FAILED ({e:#})");
                    failed.push("route chain");
                }
            }
        }
        Err(e) => {
            println!("  config file: FAILED ({e:#})");
            failed.push("config file");
        }
    }
    // Store check
    match Store::open(&home.join("state.db")) {
        Ok(s) => match s.latest_session() {
            Ok(Some(id)) => println!("  store: OK (latest session {id})"),
            Ok(None) => println!("  store: OK (empty)"),
            Err(e) => {
                println!("  store: query failed ({e})");
                failed.push("store");
            }
        },
        Err(e) => {
            println!("  store: FAILED ({e})");
            failed.push("store");
        }
    }
    // Vault: this node's keys against its member row, and the values it
    // cannot open. A regenerated identity looked like a member to every
    // check there was (pitfall 76); the resolver then fell back to the
    // env files without a word.
    match veles_core::vault::local_health() {
        Ok(None) => println!("  vault: none on this node"),
        Ok(Some(h)) => {
            let mut lines = h.lines().into_iter();
            let first = lines.next().unwrap_or_default();
            println!("  vault: {first}");
            for l in lines {
                println!("    {l}");
            }
            if h.is_problem() {
                failed.push("vault");
            }
        }
        Err(e) => {
            println!("  vault: FAILED ({e:#})");
            failed.push("vault");
        }
    }
    // What ACTUALLY reaches the model — the three figures that were
    // green and blind: memory resident vs stored, skills named vs
    // installed, and the compaction budget vs the largest session on
    // disk. None of these can fail the build; each can silently make
    // the agent dumber, which is the whole reason doctor exists.
    if let Ok(cfg) = load() {
        // Memory: the prompt injects a CAPPED slice of the store. Name
        // both numbers so "92% of memory is not in the prompt" is a
        // line you can read, not a thing you discover by guessing.
        let mem = veles_core::memory::MemoryStore::new();
        let stored_core = mem
            .read(veles_core::memory::StoreKind::Memory)
            .chars()
            .count();
        let stored_user = mem
            .read(veles_core::memory::StoreKind::User)
            .chars()
            .count();
        let prompt = veles_core::prompt::build(&cfg, None);
        let blocks = veles_core::prompt::injected_blocks(&prompt);
        let resident = |title: &str| {
            blocks
                .iter()
                .find(|(t, _)| t == title)
                .map(|(_, n)| *n)
                .unwrap_or(0)
        };
        let mem_resident = resident("MEMORY (your notes)");
        let user_resident = resident("USER PROFILE (who the user is)");
        println!(
            "  memory: core {mem_resident}/{stored_core} chars resident, \
             user {user_resident}/{stored_user} resident \
             (caps: core={}, user={})",
            cfg.memory.core_char_limit, cfg.memory.user_resident_char_limit
        );
        if stored_core > cfg.memory.core_char_limit {
            println!(
                "    NOTE: {} core chars exceed the resident cap — the rest is \
                 reachable only via the `memory` tool (action=read/search). \
                 Raise [memory] core_char_limit if it should be in every prompt.",
                stored_core - cfg.memory.core_char_limit
            );
        }
        match cfg.memory.dream_schedule.as_deref() {
            Some(s) => println!("  dream: scheduled ({s})"),
            None => println!(
                "  dream: not scheduled — set [memory] dream_schedule to run \
                 the only automatic memory writer"
            ),
        }

        // Skills: named in the prompt vs installed on disk. The gap is
        // the index cap doing its job — but a gap nobody can see is how
        // 107 of 137 skills went nameless.
        let set = veles_core::skills::SkillSet::load();
        let installed = set.skills.len();
        let recent = veles_core::prompt::recent_skill_names(&cfg);
        let index = set.index_for(&cfg.skills, &recent);
        let named = index
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .count();
        println!(
            "  skills: {named}/{installed} named in the prompt \
             (index_mode={}, cap={}, recent signal: {} skill(s))",
            cfg.skills.index_mode,
            cfg.skills.index_max_entries,
            recent.len()
        );
        if named < installed {
            println!(
                "    NOTE: {} installed skill(s) are not named — reachable via \
                 `skills_list`/`skill_view`. Raise [skills] index_max_entries \
                 to name more in every prompt.",
                installed - named
            );
        }

        // Compaction: the trigger fires on the provider's reported
        // prompt_tokens against a percent of the route's declared
        // window. A window declared at 1M with a largest session of
        // 2.5M chars means auto-compaction correctly never fires — say
        // so, rather than leaving it invisible.
        if let Ok(routes) = veles_core::providers::build_routes(&cfg) {
            if let Some(primary) = routes.first() {
                let window = primary.context_tokens.unwrap_or(0);
                let bar_tokens =
                    (window as f64 * cfg.agent.compaction.trigger_percent / 100.0) as u64;
                let bar_chars = bar_tokens.saturating_mul(4);
                // Largest LIVE (unarchived) session, measured with the
                // same estimator the trigger uses. One store handle,
                // every session — the figure is the point, so it has to
                // actually look at all of them.
                let largest = Store::open(&home.join("state.db"))
                    .map(|s| {
                        s.list_sessions(500)
                            .map(|sessions| {
                                sessions
                                    .iter()
                                    .filter_map(|(id, _)| s.load_history(id).ok())
                                    .map(|h| {
                                        veles_core::compaction::estimate_chars(
                                            &h,
                                            primary.replay_reasoning,
                                        )
                                    })
                                    .max()
                                    .unwrap_or(0)
                            })
                            .unwrap_or(0)
                    })
                    .unwrap_or(0);
                println!(
                    "  compaction: {} window, {}% trigger = ~{bar_tokens} tokens (~{bar_chars} chars); \
                     largest session ~{largest} chars",
                    if window == 0 { "unknown".to_string() } else { format!("{window}-token") },
                    cfg.agent.compaction.trigger_percent
                );
                if window > 0 && (largest as u64) < bar_chars {
                    println!(
                        "    NOTE: the largest session is under the bar — auto-compaction \
                         does not fire yet (correct, not broken). It triggers on the \
                         provider's reported tokens, which may differ from this estimate."
                    );
                }
            }
        }
    }
    // Tcl check
    match tclrt::spawn(veles_core::config::config_dir()) {
        Ok(h) => match h.eval("info patchlevel") {
            Ok(v) => println!("  tcl: {v}"),
            Err(e) => {
                println!("  tcl: eval failed ({e})");
                failed.push("tcl");
            }
        },
        Err(e) => {
            println!("  tcl: FAILED ({e})");
            failed.push("tcl");
        }
    }
    if prefix {
        print_prefix(raw)?;
    }
    if !failed.is_empty() {
        return Err(anyhow::anyhow!(
            "doctor: {} check(s) failed: {}",
            failed.len(),
            failed.join(", ")
        ));
    }
    Ok(())
}

/// `veles tools list` — every tool with its tier and schema weight.
///
/// The tiering trade made visible BEFORE you promote or demote: a core
/// tool pays its schema bytes in every prompt of every session; a
/// deferred tool pays a discovery round-trip the first time a session
/// needs it. Both costs are named here.
pub fn tools_list() -> Result<()> {
    let cfg = load()?;
    let home = veles_core::config::veles_home();
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    let registry = tools::builtin_registry(store.clone(), None, "tools-list");

    println!("[tools] tier = \"{}\"", cfg.tools.tier);
    for (name, o) in {
        let mut v: Vec<_> = cfg.tools.overrides.iter().collect();
        v.sort();
        v
    } {
        println!("  override: {name} = \"{o}\"");
    }

    let schema_len = |v: &serde_json::Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let mut rows: Vec<(String, &'static str, usize)> = Vec::new();
    let schemas = registry.openai_schemas();
    for v in &schemas {
        let name = v["function"]["name"].as_str().unwrap_or("?").to_string();
        rows.push((name, "core", schema_len(v)));
    }
    for name in registry.deferred_names() {
        let size = registry
            .schema_for(&name)
            .map(|s| serde_json::to_string(&s).map(|s| s.len()).unwrap_or(0))
            .unwrap_or(0);
        rows.push((name, "deferred", size));
    }
    for name in veles_core::deferred::deferred_names() {
        let size = veles_core::deferred::global_schema(&name)
            .map(|s| serde_json::to_string(&s).map(|s| s.len()).unwrap_or(0))
            .unwrap_or(0);
        rows.push((name, "deferred (mcp/global)", size));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));

    let mut core_bytes = 0usize;
    println!("\n{:<32} {:<20} {:>8}", "tool", "tier", "schema");
    for (name, tier, size) in &rows {
        if *tier == "core" {
            core_bytes += size;
        }
        println!("{:<32} {:<20} {:>6} B", name, tier, size);
    }
    println!(
        "\ncore tier: {} tools · {} schema bytes (≈{} tok) ride in every prompt",
        rows.iter().filter(|r| r.1 == "core").count(),
        core_bytes,
        core_bytes / 4
    );
    println!("promote one with: [tools.overrides] name = \"core\"");
    Ok(())
}

/// The fixed prefix — the bytes every LLM round pays BEFORE history.
///
/// This is the transparency surface for the cache story: the exact
/// system prompt, every injected block with its size, the tool array
/// per tool, and a BLAKE3 hash of the whole thing. Two sessions whose
/// prefixes hash differently can never share a provider cache entry,
/// so the hash is the object the hit rate turns on — and the thing a
/// mock provider can recompute to prove the surface is not lying.
fn print_prefix(raw: bool) -> Result<()> {
    let cfg = load()?;
    let prompt = veles_core::prompt::build(&cfg, None);

    let home = veles_core::config::veles_home();
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    let registry = tools::builtin_registry(store.clone(), None, "doctor-prefix");
    let schemas = registry.openai_schemas();
    let tools_json = serde_json::to_string(&schemas)?;

    if raw {
        // Verbatim: the exact bytes that ride the wire as the system
        // message. Nothing summarized, nothing elided.
        print!("{prompt}");
        return Ok(());
    }

    let prompt_chars = prompt.chars().count();
    let tools_chars = tools_json.len();
    println!();
    println!("prefix (re-sent on EVERY round, before any history):");
    println!(
        "  system prompt: {} chars (≈{} tok)",
        prompt_chars,
        prompt_chars / 4
    );
    println!(
        "  tool schemas:  {} tools · {} chars (≈{} tok)",
        schemas.len(),
        tools_chars,
        tools_chars / 4
    );

    // The tier split, because "how big is the prompt" is the less
    // useful question. What decides the bill is how much of it SURVIVES
    // a memory write or a change of directory, and that is invisible in
    // a single total.
    let (st, ctx, vol) = veles_core::prompt::tier_sizes(&cfg, None);
    println!("  prompt tiers (least volatile first — the cache keys on the prefix):");
    println!("    stable   (identity, standing guidance)  {st:>7} chars · ≈{:>5} tok — shared by EVERY session", st / 4);
    println!("    context  (cwd, git root, project file)  {ctx:>7} chars · ≈{:>5} tok — shared within a project", ctx / 4);
    println!("    volatile (memory, profile, skills)      {vol:>7} chars · ≈{:>5} tok — dies on the next memory write", vol / 4);

    // The budget the prefix feeds: the compaction bar and the warn
    // line, as configured — the same numbers /context renders against
    // live history, so the two reports read as one story.
    let cc = &cfg.agent.compaction;
    println!(
        "  compaction bar: min({}% of window, {} tokens) · warn line: {} tokens (see /context for history against both)",
        cc.trigger_percent, cc.max_context_tokens, cc.warn_context_tokens
    );

    // Per-block breakdown, read off the BUILT prompt so it cannot drift
    // from what is actually sent.
    let blocks = veles_core::prompt::injected_blocks(&prompt);
    if !blocks.is_empty() {
        println!("  injected blocks:");
        for (name, chars) in &blocks {
            println!(
                "    {:<30} {:>6} chars · ≈{:>5} tok",
                name,
                chars,
                chars / 4
            );
        }
    }

    // Per-tool schema weight, heaviest first — the cost of each tool
    // being resident, visible BEFORE deciding to promote/demote one.
    println!("  tool schemas by size:");
    let mut sizes: Vec<(String, usize)> = schemas
        .iter()
        .map(|v| {
            let name = v["function"]["name"].as_str().unwrap_or("?").to_string();
            (name, serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
        })
        .collect();
    sizes.sort_by_key(|a| std::cmp::Reverse(a.1));
    for (name, n) in &sizes {
        println!("    {:<24} {:>5} chars · ≈{:>4} tok", name, n, n / 4);
    }

    // The hash the cache story turns on: system prompt ‖ tool array.
    let mut hasher = blake3::Hasher::new();
    hasher.update(prompt.as_bytes());
    hasher.update(tools_json.as_bytes());
    println!("  prefix hash (blake3): {}", hasher.finalize().to_hex());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `doctor --sandbox` on a node whose boxes are made elsewhere
    /// (`[cluster] sandbox_home`: the phone, winbox) used to count "no
    /// container runtime here" as a problem and prescribe the very line
    /// the config had. The reading: another node's id or name → that
    /// node; unset, blank, `local`/`here` or our own id/name → boxes are
    /// made here and the round trip runs.
    #[test]
    fn boxes_elsewhere_reads_sandbox_home_the_way_the_tools_do() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let me = veles_core::node::NodeIdentity::load_or_create(&veles_core::config::config_dir())
            .expect("a scratch node identity");
        assert_eq!(boxes_elsewhere(None), None);
        assert_eq!(boxes_elsewhere(Some("")), None);
        assert_eq!(boxes_elsewhere(Some("  ")), None);
        assert_eq!(boxes_elsewhere(Some("local")), None);
        assert_eq!(boxes_elsewhere(Some("Here")), None);
        assert_eq!(boxes_elsewhere(Some(&me.id)), None, "our own id is here");
        assert_eq!(
            boxes_elsewhere(Some(" 38a5e4dd ")),
            Some("38a5e4dd".to_string()),
            "another node, trimmed"
        );
    }

    /// One control line → an answer, or the reason it is not one. A
    /// malformed `answers` used to become a fake "timed out" (the model
    /// was told nobody answered), and a multi-question JSON reply such
    /// as {"q1":"1"} — no choice/text/answers field — became an EMPTY
    /// text answer instead of reaching the reply parser.
    #[test]
    fn a_json_control_line_answers_or_says_why_not() {
        use veles_core::ask::{AskChoice, AskKind, AskQuestion, AskRequest};
        let mq = AskRequest::multi_question(
            "s1",
            AskKind::Clarify,
            vec![
                AskQuestion::new("q1", "theme?", vec![AskChoice::new("c1", "amber")]),
                AskQuestion::new("q2", "name?", vec![]),
            ],
        );
        assert!(json_reply(&mq, r#"{"answers": "yes please"}"#).is_err());
        assert!(json_reply(&mq, "sure, whatever").is_err());
        let a = json_reply(&mq, r#"{"q1":"1","q2":"Grid"}"#).unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        assert_eq!(a.per_question["q2"].free_text.as_deref(), Some("Grid"));
        let a = json_reply(&mq, r#"{"answers":{"q1":{"selected":["c1"]}}}"#).unwrap();
        assert_eq!(a.per_question["q1"].selected, vec!["c1"]);
        let single = AskRequest::new(
            "s1",
            AskKind::Approval,
            "run?",
            veles_core::ask::approval_choices(true),
        );
        assert_eq!(
            json_reply(&single, r#"{"choice":"once"}"#).unwrap().first(),
            Some("once")
        );
        assert_eq!(json_reply(&single, "y").unwrap().first(), Some("once"));
        assert!(json_reply(&single, "tell me more").is_err());
    }

    /// Doctor names a leftover `~/.veles` and never reads it; with none
    /// there it prints nothing.
    #[test]
    fn doctor_warns_about_a_leftover_legacy_home_only_when_one_exists() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(legacy_home_note(home.path()), None);
        std::fs::create_dir(home.path().join(".veles")).unwrap();
        let note = legacy_home_note(home.path()).expect("a leftover is named");
        assert!(
            note.contains(".veles") && note.contains("does not read it"),
            "{note}"
        );
    }

    #[test]
    fn json_event_serializes_stream_events() {
        let ev = StreamEvent::Route {
            provider: "ali-token-plan-personal".into(),
            model: "qwen3-max".into(),
            reasoning_effort: Some("high".into()),
        };
        let s = json_event(&ev);
        assert!(
            s.contains("\"event\":\"route\"") && s.contains("ali-token-plan"),
            "{s}"
        );
        // The MODEL has to ride along. Route used to carry the provider
        // alone, so every surface could only say "thinking… (provider)"
        // — and the model is the part that actually varies between
        // routes. USER: "should be full provider->model chain".
        assert!(
            s.contains("\"model\":\"qwen3-max\""),
            "the route event must name the model too: {s}"
        );
        let ev = StreamEvent::Delta("hi".into());
        let s = json_event(&ev);
        assert!(
            s.contains("\"event\":\"delta\"") && s.contains("\"text\":\"hi\""),
            "{s}"
        );
        let ev = StreamEvent::ToolStart("terminal".into(), "{\"command\":\"ls\"}".into());
        let s = json_event(&ev);
        assert!(s.contains("tool_start") && s.contains("terminal"), "{s}");
        let ev = StreamEvent::Cancelled;
        assert_eq!(json_event(&ev), "{\"event\":\"cancelled\"}");
    }

    #[test]
    fn json_ask_event_carries_kind_and_choices() {
        let req = veles_core::ask::AskRequest::new(
            "sess",
            veles_core::ask::AskKind::Failover,
            "failover?",
            vec![
                veles_core::ask::AskChoice::new("retry", "retry"),
                veles_core::ask::AskChoice::new("next", "next"),
            ],
        );
        let s = json_event(&StreamEvent::Ask(Box::new(req)));
        assert!(s.contains("\"kind\":\"failover\""), "{s}");
        assert!(s.contains("\"choices\""), "{s}");
    }

    #[test]
    fn unknown_slash_refusal_is_narrow() {
        // The typo shape: bare lowercase word, no arguments. The
        // near-miss must name /crons for /crns.
        let r = unknown_slash_refusal("/crns", "").expect("a bare typo is refused");
        assert!(r.contains("unknown command: /crns"), "{r}");
        assert!(r.contains("/crons"), "the near-miss must name it: {r}");

        // A leading path is a PROMPT, not a typo — it must pass
        // through to the model. So must any word with arguments (a
        // custom command's invocation shape) and any word that is not
        // the command grammar (uppercase, digits first).
        assert!(unknown_slash_refusal("/home/user/report.md", "").is_none());
        assert!(unknown_slash_refusal("/dev/null", "").is_none());
        assert!(unknown_slash_refusal("/crns", "src/main.rs").is_none());
        assert!(unknown_slash_refusal("/CRNS", "").is_none());
        assert!(unknown_slash_refusal("/9cron", "").is_none());
        // The refusal SAYS it did not bill a turn.
        assert!(r.contains("not sent to the model"), "{r}");
    }

    #[test]
    fn a_custom_command_is_not_refused() {
        // find_any walks `.veles/commands/` under cwd and the home —
        // pin both by chdir, so the refusal's "one more chance to be
        // real" lookup is exercised against a command that EXISTS.
        let _g = veles_core::testenv::lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::current_dir().ok();
        std::env::set_current_dir(dir.path()).expect("chdir");
        std::fs::create_dir_all(dir.path().join(".veles/commands")).unwrap();
        std::fs::write(
            dir.path().join(".veles/commands/deploy.md"),
            "# /deploy\nship it",
        )
        .unwrap();
        assert!(
            unknown_slash_refusal("/deploy", "").is_none(),
            "a custom command must reach its dispatcher, not the refusal"
        );
        assert!(unknown_slash_refusal("/deployy", "").is_some());
        if let Some(p) = prev {
            let _ = std::env::set_current_dir(p);
        }
    }
}
