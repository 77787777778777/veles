//! `veles chat` — the interactive CLI surface, hermes-CLI parity target.
//!
//! What hermes' CLI does that this now does:
//!   - markdown-rendered replies (termimad), streamed deltas while running
//!   - Ctrl-C interrupts a running turn (hermes /stop parity); two idle
//!     Ctrl-Cs at the prompt quit
//!   - slash commands: /help /new /clear /retry /undo /title /sessions
//!     /resume /model /reasoning /usage /status /tools /plugins
//!     /quit [--delete]
//!   - session persistence + resume (--continue / --resume <id>)
//!   - status line after each turn: route · tokens · tool rounds
//!
//! Threading model: rustyline owns the main thread; each turn runs on a
//! dedicated thread with its own tokio runtime, so SIGINT can set the
//! cancel token while the stream is in flight (the main thread is blocked
//! in the turn join, not in readline).

use crate::{cur, AgentHandle};
use anyhow::Result;
use rustyline::error::ReadlineError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use veles_core::agent::{Agent, TurnResult};
use veles_core::config::load;
use veles_core::hooks::HookBus;
use veles_core::lockutil::{LockOk, RwLockOk};
use veles_core::providers::{CancelToken, ChatMessage, StreamEvent};
use veles_core::store::Store;
use veles_core::tclrt;
use veles_core::tools;

/// Outcome of running one user prompt on the turn thread.
enum TurnOutcome {
    Done(TurnResult),
    Failed(String),
}

pub use veles_core::store::new_session_id;

/// Shared boot path (also used by one-shot + TUI): config + Tcl + hooks +
/// tools + store + agent.
pub fn boot(session_id: String) -> Result<Agent> {
    let cfg = Arc::new(load()?);
    let home = veles_core::config::veles_home();
    let tcl = tclrt::spawn(veles_core::config::config_dir())?;
    let hooks = HookBus::new(Some(tcl));
    let store = Arc::new(Mutex::new(Store::open(&home.join("state.db"))?));
    let registry = Arc::new(tools::builtin_registry(store.clone(), None, &session_id));
    store.lock_ok().create_session(&session_id)?;
    let agent = Agent::new(cfg.clone(), hooks, registry, store, session_id)?;
    // A delegate child is not the owner surface its binary would be:
    // `delegate.rs` passes the CALLER's principal in the environment,
    // and binding it here is what makes the media/F1 gates hold inside
    // the child. Unset (the normal CLI case) changes nothing. A set but
    // unreadable var stops the boot — see `acl::principal_from_env`
    // for why falling back to owner would be the hole reopening.
    match veles_core::acl::principal_from_env() {
        Ok(Some(p)) => agent.set_principal(p),
        Ok(None) => {}
        Err(e) => anyhow::bail!(e),
    }
    // MCP tools (gateway parity) into the deferred registry.
    crate::cli::register_mcp_deferred(&cfg);
    Ok(agent)
}

fn resolve_start_session(resume: bool, explicit: Option<&str>) -> Result<(String, bool)> {
    // Returns (session_id, load_history)
    let home = veles_core::config::veles_home();
    let store = Store::open(&home.join("state.db"))?;
    if let Some(id) = explicit {
        // Allow either a full id or a prefix, over every session.
        if let Some(full) = store.find_session(id)? {
            return Ok((full, true));
        }
        anyhow::bail!("no session matching '{id}'");
    }
    if resume {
        if let Some(id) = store.latest_session()? {
            return Ok((id, true));
        }
    }
    Ok((new_session_id(), false))
}

fn banner(agent: &Agent) {
    // What the session will actually send (one helper for every
    // surface: an override that names only a provider still runs the
    // route the agent resolves, not `override or default` per field).
    let (provider, model, _, _) = agent.effective_model();
    let or_q = |s: String| if s.is_empty() { "?".to_string() } else { s };
    let (provider, model) = (or_q(provider), or_q(model));
    // Synthwave wordmark (ANSI), then the facts line.
    print!(
        "{}",
        veles_core::art::render_ansi(&veles_core::art::wordmark())
    );
    println!(
        "veles {} · rust core, tcl soul\nmodel: {} @ {} · session: {}\n/help for commands, Ctrl-C to interrupt, Ctrl-D to quit.\n",
        veles_core::VERSION, model, provider, agent.session_id
    );
}

/// The local clock as the REPL stamps its event lines: every provider
/// call's route line, every tool start and every note says WHEN (USER
/// 2026-09-29: "it's not always obvious to me from the context when the
/// messages were sent").
fn stamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn print_event(ev: &StreamEvent, show_reasoning: bool) {
    match ev {
        StreamEvent::Route {
            provider, model, ..
        } => eprintln!("[{} route: {provider}/{model}]", stamp()),
        StreamEvent::Delta(t) => {
            use std::io::Write;
            print!("{t}");
            let _ = std::io::stdout().flush();
        }
        StreamEvent::ReasoningDelta(_) if show_reasoning => {
            // Progress ticker only — the reasoning-summary plugin handles
            // display; streaming the raw chain-of-thought would flood.
            // a11y linear mode (VELES_PLAIN=1): text ticker instead of
            // dots, so screen readers see activity.
            use std::io::Write;
            if std::env::var("VELES_PLAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                print!(".");
            } else {
                print!("·");
            }
            let _ = std::io::stdout().flush();
        }
        StreamEvent::ReasoningDelta(_) => {}
        // a11y: [tool name] markers instead of glyphs in plain mode —
        // screen readers and greppable logs both read these natively.
        StreamEvent::ToolStart(name, _args) => {
            if std::env::var("VELES_PLAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                eprintln!("  {} [tool] {name} starting", stamp());
            } else {
                eprintln!("  {} ⚙ {name}…", stamp());
            }
        }
        StreamEvent::ToolEnd(name, result) => {
            let truncated = if result.len() > 200 { "…" } else { "" };
            if std::env::var("VELES_PLAIN")
                .map(|v| v == "1")
                .unwrap_or(false)
            {
                eprintln!("  [tool] {name} done ({} bytes){}", result.len(), truncated);
            } else {
                eprintln!("  ✓ {name} ({} bytes){}", result.len(), truncated);
            }
        }
        // Questions are drawn by the REPL's own ask pump (prompt_ask),
        // which is the thing that can also read the answer — rendering
        // them here too would print the prompt twice.
        StreamEvent::Ask(_) => {}
        StreamEvent::Done(_) => println!(),
        StreamEvent::Cancelled => eprintln!("\n[interrupted]"),
        // Never silent: a guard firing or history being rewritten is
        // something the user must be able to see happen.
        StreamEvent::Note(n) => {
            eprintln!("\n  {} {}", stamp(), n.line());
            if !n.detail.is_empty() {
                for l in n.detail.lines() {
                    eprintln!("      {l}");
                }
            }
            if let Some(t) = &n.toggle {
                eprintln!("      ({} = {} — /mechanisms to change)", t.key, t.value);
            }
        }
    }
}

// No `#[allow(clippy::await_holding_lock)]` here any more: the turn
// clones the `Arc<Agent>` out of the handle and runs on `&self`, so no
// guard spans the await. See `frontends::AgentHandle`.
fn run_turn_on_thread(
    agent: AgentHandle,
    history: Arc<Mutex<Vec<ChatMessage>>>,
    text: String,
    cancel: CancelToken,
) -> std::thread::JoinHandle<TurnOutcome> {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => return TurnOutcome::Failed(format!("runtime: {e}")),
        };
        let agent = cur(&agent);
        let show_reasoning = agent.overrides.read_ok().show_reasoning;
        let res = rt.block_on(async {
            let mut h = history.lock_ok().clone();
            let outcome = {
                agent
                    .run_turn(&mut h, &text, Some(cancel), |ev| {
                        print_event(&ev, show_reasoning)
                    })
                    .await
            };
            // Write the advanced history back regardless of outcome so an
            // interrupted turn keeps whatever arrived.
            *history.lock_ok() = h;
            outcome
        });
        match res {
            Ok(tr) => TurnOutcome::Done(tr),
            Err(e) => TurnOutcome::Failed(format!("{e:#}")),
        }
    })
}

/// The REPL's end of the ask channel. A process has at most one REPL, so
/// a static keeps `/retry` and the main loop on the same queue without
/// threading a receiver through every slash-command signature.
static ASK_RX: std::sync::OnceLock<Mutex<std::sync::mpsc::Receiver<veles_core::ask::AskRequest>>> =
    std::sync::OnceLock::new();

/// Give this agent a terminal to ask questions through.
fn install_terminal_asker(agent: &mut Agent) {
    let (tx, rx) = std::sync::mpsc::channel::<veles_core::ask::AskRequest>();
    if ASK_RX.set(Mutex::new(rx)).is_err() {
        return; // already installed (second REPL in-process: ignore)
    }
    agent.set_asker(Arc::new(veles_core::ask::ChannelAsker { tx }));
}

/// Wait for the turn thread while staying able to answer its questions.
///
/// `handle.join()` used to block here, which meant an approval prompt
/// could never be answered: the only thread that can read stdin was
/// parked. Now we poll the ask channel and fall back to `is_finished`.
fn pump_turn(handle: std::thread::JoinHandle<TurnOutcome>) -> std::result::Result<TurnOutcome, ()> {
    use std::sync::mpsc::RecvTimeoutError;
    if let Some(rx) = ASK_RX.get() {
        let rx = rx.lock_ok();
        loop {
            match rx.recv_timeout(std::time::Duration::from_millis(80)) {
                Ok(req) => prompt_ask(&req),
                Err(RecvTimeoutError::Timeout) => {
                    if handle.is_finished() {
                        break;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }
    handle.join().map_err(|_| ())
}

/// Draw one question on the terminal and resolve it. Plain stdin, not
/// rustyline: this runs mid-turn, where the editor is not active.
fn prompt_ask(req: &veles_core::ask::AskRequest) {
    use std::io::Write;
    println!("\n{}", veles_core::ask::render_prompt(req));
    loop {
        print!("answer> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                // EOF/error at an approval prompt means "no".
                veles_core::ask::resolve(
                    &req.id,
                    veles_core::ask::AskAnswer::choice(veles_core::ask::CHOICE_DENY),
                );
                return;
            }
            Ok(_) => {
                if let Some(a) = veles_core::ask::parse_reply(req, &line) {
                    veles_core::ask::resolve(&req.id, a);
                    return;
                }
                if req.choices.is_empty() && !line.trim().is_empty() {
                    veles_core::ask::resolve(
                        &req.id,
                        veles_core::ask::AskAnswer::text(line.trim()),
                    );
                    return;
                }
                println!("(answer with one of the options above)");
            }
        }
    }
}

fn status_line(r: &TurnResult) -> String {
    let mut s = format!(
        "─ {} · {} · {} tok ({}+{})",
        r.provider,
        r.model,
        r.prompt_tokens + r.completion_tokens,
        r.prompt_tokens,
        r.completion_tokens
    );
    // The honest cost metric: what was NOT served from cache. Shown
    // only when the provider reports a cache field — printing 0 for a
    // provider that never reports one would read as "nothing cached".
    if let Some(u) = r.uncached_prompt_tokens() {
        s.push_str(&format!(" · {u} uncached"));
    }
    // Reasoning, when the provider splits it out: a route that folds
    // thinking into completion reports nothing here, so the line shows
    // nothing rather than a misleading "0 thinking". When it does split
    // them, the completion figure above is really answer + thinking, so
    // name both — the point of the separate count is to make an
    // overthinking model legible instead of just expensive-looking.
    if let Some(rt) = r.reasoning_tokens {
        let answer = r.completion_tokens.saturating_sub(rt);
        s.push_str(&format!(" · {answer} answer · {rt} thinking"));
    }
    if r.tool_rounds > 0 {
        s.push_str(&format!(" · {} tool round(s)", r.tool_rounds));
    }
    if r.interrupted {
        s.push_str(" · interrupted");
    }
    s
}

fn parse_input(line: &str) -> ReplInput {
    if !line.starts_with('/') {
        return ReplInput::Prompt(line.to_string());
    }
    let lower = line.to_lowercase();
    if lower.starts_with("/quit") || lower.starts_with("/exit") {
        return ReplInput::Quit {
            delete: lower.contains("--delete"),
        };
    }
    ReplInput::Slash(line.to_string())
}

enum ReplInput {
    Prompt(String),
    Slash(String),
    Quit { delete: bool },
}

#[cfg(test)]
impl ReplInput {
    fn is_prompt(&self, s: &str) -> bool {
        matches!(self, ReplInput::Prompt(t) if t == s)
    }
    fn is_slash(&self, s: &str) -> bool {
        matches!(self, ReplInput::Slash(t) if t == s)
    }
    fn is_quit(&self, delete: bool) -> bool {
        matches!(self, ReplInput::Quit { delete: d } if *d == delete)
    }
}

// ── the REPL's readline helper ───────────────────────────────────────
//
// The REPL was a bare `rustyline::DefaultEditor::new()` — no completer at
// all — so Tab did nothing and the only way to find a command was to
// type `/help` and read a wall of text. The TUI has had an M-x palette
// and the desktop a popup for rounds; this is the third surface closing
// the same gap, and it reads the SAME table (`commands::help_entries`),
// so a command added in one place shows up in all three.
//
// Two rustyline seams do the work:
//
//   * `Completer` — what Tab offers. Candidates are `Pair`s: the
//     `display` is what the list shows (name + description) and the
//     `replacement` is what lands in the buffer (the bare name), so
//     accepting never pastes the prose into the prompt.
//   * `Hinter` — the ghost text to the right of the cursor. With one
//     candidate it shows the rest of the name; with a full command typed
//     it shows what that command DOES, which is the same bargain the
//     desktop's doc line and the palette's description column make.
//
// Highlighting is left to the default: a coloured prompt would fight
// termimad's own output on the same terminal.

/// Command completion for the REPL prompt.
struct ReplHelper {
    /// The shared table, resolved once at startup: `/model [name]` →
    /// (label, description). Built from `help_entries(Surface::Repl)`,
    /// so this cannot drift from `/help` or from the other surfaces.
    commands: Vec<(String, String)>,
    /// Model aliases and declared commands from the config, as a second
    /// group of Tab candidates (`/qwen-flash`, `/my-image-alias`).
    aliases: Vec<(String, String)>,
}

impl ReplHelper {
    fn new(cfg: &veles_core::config::VelesConfig) -> Self {
        let commands = super::commands::help_entries(super::commands::Surface::Repl)
            .into_iter()
            .map(|(usage, desc)| (usage.to_string(), desc.to_string()))
            .collect();
        let aliases = veles_core::aliases::table(cfg)
            .into_iter()
            .map(|a| (format!("/{}", a.name), a.summary().to_string()))
            .collect();
        Self { commands, aliases }
    }

    /// The word the cursor is in, as `(start_offset, word)`.
    ///
    /// `pos` comes from rustyline and is a char boundary inside `line`,
    /// but slicing on that assumption is a panic in the one place a user
    /// is typing — so the offset is clamped and walked back to a
    /// boundary. A wrong answer here costs a completion; a panic costs
    /// the session.
    fn word_at(line: &str, pos: usize) -> (usize, &str) {
        let mut end = pos.min(line.len());
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        let before = &line[..end];
        let start = before
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        (start, &before[start..])
    }

    /// The head (`/model`) of every candidate, aliases included.
    fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .commands
            .iter()
            .chain(self.aliases.iter())
            .map(|(label, _)| label.split_whitespace().next().unwrap_or(label).to_string())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// What a fully-typed command does, for the ghost line.
    fn describe(&self, word: &str) -> Option<&str> {
        self.commands
            .iter()
            .chain(self.aliases.iter())
            .find(|(label, _)| label.split_whitespace().next() == Some(word))
            .map(|(_, desc)| desc.as_str())
    }

    /// Candidates for the word being completed.
    ///
    /// A slash word completes commands. Anything else gets no candidates
    /// at all — a REPL prompt is mostly prose, and hijacking Tab to
    /// complete English would be worse than doing nothing (the desktop's
    /// word-completion has a whole UI to disambiguate with; this does
    /// not).
    fn candidates(&self, word: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        if !word.starts_with('/') {
            return out;
        }
        for (label, desc) in self.commands.iter().chain(self.aliases.iter()) {
            let head = label.split_whitespace().next().unwrap_or(label);
            if head.starts_with(word) && head != word {
                out.push((label.clone(), desc.clone()));
            }
        }
        out
    }
}

impl rustyline::completion::Completer for ReplHelper {
    type Candidate = rustyline::completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Self::Candidate>)> {
        let (start, word) = Self::word_at(line, pos);
        let pairs = self
            .candidates(word)
            .into_iter()
            .map(|(label, desc)| rustyline::completion::Pair {
                // The list shows what the command IS and what it DOES;
                // the buffer only ever receives the name.
                display: if desc.is_empty() {
                    label.clone()
                } else {
                    format!("{label:<26} {desc}")
                },
                replacement: label
                    .split_whitespace()
                    .next()
                    .unwrap_or(&label)
                    .to_string(),
            })
            .collect();
        Ok((start, pairs))
    }
}

impl rustyline::hint::Hinter for ReplHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, _ctx: &rustyline::Context<'_>) -> Option<String> {
        let (_, word) = Self::word_at(line, pos);
        if !word.starts_with('/') {
            return None;
        }
        // Something typed is a prefix: show the rest of the name. An
        // exact head gets its description instead — the point at which
        // "which command was that" becomes "what does this one do".
        if let Some(rest) = self
            .names()
            .into_iter()
            .find(|n| n.starts_with(word) && n.len() > word.len())
        {
            return Some(rest[word.len()..].to_string());
        }
        self.describe(word).map(|d| format!("  {d}"))
    }
}

impl rustyline::highlight::Highlighter for ReplHelper {}
impl rustyline::validate::Validator for ReplHelper {}
impl rustyline::Helper for ReplHelper {}

pub fn run(
    resume: bool,
    explicit_session: Option<&str>,
    ov: &veles_core::agent::ModelOverrides,
) -> Result<()> {
    let (session_id, load_hist) = resolve_start_session(resume, explicit_session)?;
    let agent = boot(session_id)?;
    // The flags reach the REPL now. They used to stop at main.rs.
    for w in agent.apply_overrides(ov) {
        eprintln!("warning: {w}");
    }
    let resume_history = if load_hist {
        agent
            .store
            .lock()
            .unwrap()
            .load_history(&agent.session_id)?
    } else {
        Vec::new()
    };
    banner(&agent);
    // The REPL can answer questions (approvals, `clarify`): the turn
    // thread posts them and this thread prompts for the answer.
    let mut agent = agent;
    install_terminal_asker(&mut agent);
    let agent: AgentHandle = super::handle(agent);
    let history = Arc::new(Mutex::new(resume_history));

    let mut rl = rustyline::Editor::<ReplHelper, rustyline::history::DefaultHistory>::new()?;
    rl.set_helper(Some(ReplHelper::new(&cur(&agent).cfg)));
    // Tab completion + ghost hints for slash commands (see ReplHelper).
    // A history FILE, too: the REPL kept history only in memory, so
    // every restart lost it — and completion built on history would
    // have been building on nothing.
    let hist_dir = veles_core::config::data_dir().join("history");
    let _ = std::fs::create_dir_all(&hist_dir);
    let hist_file = hist_dir.join("repl.txt");
    let _ = rl.load_history(&hist_file);

    // SIGINT: cancel token during turns (flag only arms while a turn runs;
    // idle Ctrl-C falls through to rustyline's Interrupted handling).
    let turn_active = Arc::new(AtomicBool::new(false));
    let cancel = CancelToken::new();
    {
        let cancel = cancel.clone();
        let active = turn_active.clone();
        // low_level::register takes a closure (flag::register only accepts
        // a plain AtomicBool flag). Unsafe because the handler runs in
        // signal context — ours only touches async-signal-safe atomics.
        let _sig = unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
                if active.load(Ordering::SeqCst) {
                    cancel.cancel();
                }
            })
        };
    }

    let mut consecutive_idle_ctrlc = 0u8;

    // Session mail (veles_core::mail): this process's socket, and its
    // session as one siblings can list and nudge. Readline owns this
    // thread, so a nudge only prints a line (through rustyline's
    // external printer, which redraws the prompt under it); the next
    // Enter — an empty one does it — runs what waits in the queue.
    veles_core::runsock::serve("repl");
    let printer: Option<Box<dyn rustyline::ExternalPrinter + Send>> = rl
        .create_external_printer()
        .ok()
        .map(|p| Box::new(p) as Box<dyn rustyline::ExternalPrinter + Send>);
    let presence_key = veles_core::presence::register(Arc::new(ReplPresence {
        agent: agent.clone(),
        printer: printer.map(Mutex::new),
    }));
    // Anything already waiting (mail sent while nobody had this session
    // open) runs before the first prompt.
    drain_queue(&agent, &history, &cancel, &turn_active);

    loop {
        let readline = {
            turn_active.store(false, Ordering::SeqCst);
            rl.readline("veles> ")
        };
        let line = match readline {
            Ok(l) => {
                consecutive_idle_ctrlc = 0;
                l.trim().to_string()
            }
            Err(ReadlineError::Interrupted) => {
                consecutive_idle_ctrlc += 1;
                if consecutive_idle_ctrlc >= 2 {
                    println!("(bye)");
                    break;
                }
                println!("(Ctrl-C again to quit)");
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };
        if line.is_empty() {
            // An empty Enter runs whatever waits in the queue.
            drain_queue(&agent, &history, &cancel, &turn_active);
            continue;
        }
        let _ = rl.add_history_entry(&line);

        match parse_input(&line) {
            ReplInput::Quit { delete } => {
                if delete {
                    let sid = cur(&agent).session_id.clone();
                    cur(&agent).store.lock().unwrap().delete_session(&sid).ok();
                    println!("(session deleted)");
                }
                break;
            }
            ReplInput::Slash(cmd) => {
                if let Err(e) = handle_slash(&cmd, &agent, &history) {
                    eprintln!("error: {e:#}");
                }
            }
            ReplInput::Prompt(text) => {
                run_prompt(&agent, &history, text, &cancel, &turn_active);
                // The turn boundary drains the shared queue, as every
                // other surface does: text parked from another surface,
                // and mail, run next.
                drain_queue(&agent, &history, &cancel, &turn_active);
            }
        }
    }
    veles_core::presence::unregister(presence_key);
    veles_core::runsock::shutdown();
    let _ = rl.save_history(&hist_file);
    Ok(())
}

/// One turn on its own thread, its reply printed.
fn run_prompt(
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<ChatMessage>>>,
    text: String,
    cancel: &CancelToken,
    turn_active: &Arc<AtomicBool>,
) {
    turn_active.store(true, Ordering::SeqCst);
    cancel.reset();
    // No last_prompt bookkeeping here: run_turn remembers
    // what was sent, for every surface alike.
    let handle = run_turn_on_thread(agent.clone(), history.clone(), text, cancel.clone());
    // Don't block on join(): the turn may need to ask us
    // something, and only this thread can read stdin.
    match pump_turn(handle) {
        Ok(TurnOutcome::Done(tr)) => {
            if !tr.reply.is_empty() {
                println!();
                let skin = termimad::MadSkin::default_dark();
                skin.print_text(&tr.reply);
            } else if let Some(n) = tr.empty_reply_notice() {
                eprintln!("\n[veles] {n}");
            }
            println!("{}", status_line(&tr));
        }
        Ok(TurnOutcome::Failed(e)) => eprintln!("\nerror: {e}"),
        Err(_) => eprintln!("\nturn thread panicked"),
    }
    turn_active.store(false, Ordering::SeqCst);
}

/// Run every row waiting in this session's queue, oldest first — text
/// another surface parked, and mail (painted with its sender; the turn
/// gets the whole body, which `run_turn` claims).
fn drain_queue(
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<ChatMessage>>>,
    cancel: &CancelToken,
    turn_active: &Arc<AtomicBool>,
) {
    loop {
        let (sid, item) = {
            let a = cur(agent);
            let sid = a.session_id.clone();
            let item = a.store.lock_ok().queue_take_next(&sid).ok().flatten();
            (sid, item)
        };
        let Some(item) = item else { break };
        match (item.kind.as_deref() == Some("mail"))
            .then(|| veles_core::mail::parse_body(&item.body))
            .flatten()
        {
            Some((id, text)) => {
                let from = cur(agent)
                    .store
                    .lock_ok()
                    .mail_get(id)
                    .ok()
                    .flatten()
                    .and_then(|r| r.from_session)
                    .unwrap_or_else(|| "the command line".to_string());
                println!("\n✉ mail from {from}:\n{text}");
            }
            None => println!("\n(queued) {}", item.body),
        }
        run_prompt(agent, history, item.body, cancel, turn_active);
        let _ = cur(agent).store.lock_ok().queue_release(&sid, item.seq);
    }
}

/// The REPL's session, as one this process holds open (session mail).
struct ReplPresence {
    agent: AgentHandle,
    printer: Option<Mutex<Box<dyn rustyline::ExternalPrinter + Send>>>,
}

impl veles_core::presence::PresenceSource for ReplPresence {
    fn open_sessions(&self) -> Vec<veles_core::presence::OpenSession> {
        vec![veles_core::presence::OpenSession {
            session_id: cur(&self.agent).session_id.clone(),
            surface: "repl".into(),
        }]
    }

    fn nudge(&self, session_id: &str) -> bool {
        if cur(&self.agent).session_id != session_id {
            return false;
        }
        let line = "✉ mail arrived for this session — press Enter to run it".to_string();
        match &self.printer {
            Some(p) => {
                let _ = p.lock_ok().print(line);
            }
            None => eprintln!("{line}"),
        }
        true
    }
}

fn help() {
    // One generated table (RM4.4) — the same listing every surface
    // renders, plus the terminal-only rows. See commands::help_entries.
    println!(
        "commands\n{}",
        super::commands::help_text(super::commands::Surface::Repl)
    );
}

fn handle_slash(
    cmd: &str,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<ChatMessage>>>,
) -> Result<()> {
    let mut parts = cmd.splitn(2, ' ');
    let head = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();

    // Shared engine-state commands first, so the REPL cannot drift from
    // the TUI and the gateway again. See frontends/commands.rs.
    {
        let a = cur(agent);
        let mut h = history.lock_ok();
        let mut ctx = super::commands::CommandCtx {
            agent: &a,
            surface: super::commands::Surface::Repl,
            history: Some(&mut *h),
            // The REPL is synchronous: a turn has always finished
            // before the prompt returns, so no command can race one.
            turn_live: false,
            principal: None,
        };
        if let Some(out) = super::commands::dispatch(&mut ctx, head, rest) {
            println!("{}", out.text);
            return Ok(());
        }
    }

    // A CHAT alias — `/qwen-flash <prompt>`. Beside `/new` for the same
    // reason `/new` is here and not in the shared dispatcher: it rebinds
    // the handle, which `CommandCtx`'s `&Agent` cannot do. Media aliases
    // need none of this and were already answered by `dispatch` above.
    if let Some(al) = veles_core::aliases::resolve_command(&cur(agent).cfg, head) {
        if al.kind == veles_core::aliases::AliasKind::Chat {
            let id = new_session_id();
            cur(agent).store.lock_ok().create_session(&id)?;
            super::rebind(agent, &id)?;
            history.lock_ok().clear();
            // On the FRESH agent — `rebind` forks, and a fork carries the
            // previous session's overrides forward, so setting this on
            // the old one leaves the new session on the old route.
            let route = match cur(agent).set_model(&al.model_arg()) {
                Ok(m) => m,
                Err(e) => {
                    println!("(new session {id}, but the route did not resolve: {e:#})");
                    return Ok(());
                }
            };
            if let Some(r) = &al.reasoning {
                cur(agent).set_reasoning(r);
            }
            println!("(new session {id} · {route})");
            if !rest.trim().is_empty() {
                run_prompt_once(agent, history, rest.to_string())?;
            }
            return Ok(());
        }
    }

    match head {
        "/help" | "/?" => help(),
        "/new" | "/reset" => {
            let id = new_session_id();
            cur(agent).store.lock_ok().create_session(&id)?;
            // REBIND, not `a.session_id = id`: the tool registry is
            // session-scoped, so assigning the field moved the
            // conversation and left todo/clarify/session_search still
            // writing to the session the user had just left.
            super::rebind(agent, &id)?;
            history.lock_ok().clear();
            println!("(new session {id})");
        }
        "/retry" => {
            let text = cur(agent).last_prompt();
            let Some(text) = text else {
                println!("nothing to retry");
                return Ok(());
            };
            println!("(retrying last prompt…)");
            run_prompt_once(agent, history, text)?;
        }
        "/resume" => {
            if rest.is_empty() {
                println!("usage: /resume <id-or-prefix>");
                return Ok(());
            }
            let a = cur(agent);
            let found = {
                let store = a.store.lock_ok();
                store.find_session(rest)?
            };
            match found {
                Some(id) => {
                    let h = a.store.lock_ok().load_history(&id)?;
                    drop(a);
                    // Rebind for the reason /new does.
                    super::rebind(agent, &id)?;
                    *history.lock_ok() = h.clone();
                    println!("(resumed {id}, {} messages)", h.len());
                }
                None => println!("no session matching '{rest}'"),
            }
        }
        "/verbose" => {
            println!("(verbose is always on in the CLI — tool progress is shown inline)");
        }
        "/copy" => {
            let a = cur(agent);
            let msgs = a.store.lock_ok().load_history(&a.session_id)?;
            let last_reply = msgs
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| m.content.clone())
                .unwrap_or_default();
            if last_reply.is_empty() {
                println!("(nothing to copy)");
            } else {
                // Try xclip, then xsel, then wl-copy
                let clip = |tool: &str| {
                    std::process::Command::new(tool)
                        .stdin(std::process::Stdio::piped())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn()
                        .and_then(|mut c| {
                            use std::io::Write;
                            c.stdin.as_mut().unwrap().write_all(last_reply.as_bytes())?;
                            c.wait()
                        })
                        .map(|_| true)
                        .unwrap_or(false)
                };
                if clip("xclip") || clip("xclip-selection") || clip("xsel") || clip("wl-copy") {
                    println!("(copied {} chars to clipboard)", last_reply.len());
                } else {
                    println!("(no clipboard tool — install xclip or wl-copy)");
                }
            }
        }
        "/paste" => {
            let text = veles_core::clipboard::read();
            match text {
                Some(t) if !t.is_empty() => {
                    println!("(pasting from clipboard: {t:.80}…)");
                    run_prompt_once(agent, history, t)?;
                }
                _ => println!("(clipboard empty or no clipboard tool)"),
            }
        }
        "/redraw" => {
            // CLI redraws every prompt anyway
            println!("(CLI redraws every prompt — nothing to do)");
        }
        other => {
            // S6: custom commands from .veles/commands/*.md — render the
            // template (args, shell, file inlines) and run it as a turn.
            // Plugin-registered commands (veles::register_command) hop
            // through the plugins interpreter — sync hop, the chat main
            // loop is not inside a runtime here.
            let (name, args) = match other.split_once(' ') {
                Some((n, a)) => (n, a),
                None => (other, ""),
            };
            match veles_core::commands::find_any(name) {
                Some(veles_core::commands::ResolvedCommand::Disk(c)) => {
                    let prompt = veles_core::commands::render(&c, args);
                    println!("(custom command /{} from {})", c.name, c.source.display());
                    run_prompt_once(agent, history, prompt)?;
                }
                Some(veles_core::commands::ResolvedCommand::Plugin(t)) => {
                    match veles_core::commands::render_plugin_blocking(&t, args) {
                        Ok(prompt) => {
                            println!("(plugin command /{})", t.name);
                            run_prompt_once(agent, history, prompt)?;
                        }
                        Err(e) => println!("(command /{} failed: {e})", t.name),
                    }
                }
                None => println!("unknown command: {other} (try /help)"),
            }
        }
    }
    Ok(())
}

/// Shared prompt execution used by the main loop and /retry.
fn run_prompt_once(
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<ChatMessage>>>,
    text: String,
) -> Result<()> {
    let cancel = CancelToken::new();
    let handle = run_turn_on_thread(agent.clone(), history.clone(), text, cancel);
    match pump_turn(handle) {
        Ok(TurnOutcome::Done(tr)) => {
            if !tr.reply.is_empty() {
                println!();
                let skin = termimad::MadSkin::default_dark();
                skin.print_text(&tr.reply);
            } else if let Some(n) = tr.empty_reply_notice() {
                eprintln!("\n[veles] {n}");
            }
            println!("{}", status_line(&tr));
        }
        Ok(TurnOutcome::Failed(e)) => eprintln!("\nerror: {e}"),
        Err(_) => eprintln!("\nturn thread panicked"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use veles_core::agent::TurnResult;

    // ── the REPL's completion (PENDING's residual, third surface) ────

    fn helper() -> ReplHelper {
        ReplHelper::new(&veles_core::config::VelesConfig::default())
    }

    /// A `Context` borrows a history, so it cannot be built inline from a
    /// temporary — the binding has to outlive the context.
    fn hint_of(line: &str, pos: usize) -> Option<String> {
        let h = helper();
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        rustyline::hint::Hinter::hint(&h, line, pos, &ctx)
    }

    /// `complete` returns (start, candidates). These cases call it the
    /// way rustyline does — with the cursor at the end of what was typed.
    fn complete_at(line: &str) -> (usize, Vec<rustyline::completion::Pair>) {
        let h = helper();
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        rustyline::completion::Completer::complete(&h, line, line.len(), &ctx).unwrap()
    }

    /// The candidates' inserted text, for messages that cannot use `{:?}`
    /// (`Pair` is not `Debug`).
    fn replacements(pairs: &[rustyline::completion::Pair]) -> Vec<&str> {
        pairs.iter().map(|p| p.replacement.as_str()).collect()
    }

    /// The residual, verbatim: "the REPL is a bare
    /// `rustyline::DefaultEditor::new()`". Tab did nothing. This pins
    /// that it now offers the shared command table.
    #[test]
    fn tab_offers_the_shared_command_table() {
        let (start, pairs) = complete_at("/mo");
        assert_eq!(start, 0, "the word starts at the beginning of the line");
        assert!(
            pairs.iter().any(|p| p.replacement == "/model"),
            "Tab must offer /model: {:?}",
            replacements(&pairs)
        );
    }

    /// The candidate SHOWS what the command does. rustyline lists
    /// `display` and inserts `replacement`, so the description can ride
    /// along without ending up in the prompt — which is exactly the
    /// split this test exists to keep honest.
    #[test]
    fn a_candidate_shows_its_description_but_inserts_only_the_name() {
        let (_, pairs) = complete_at("/quo");
        let quote = pairs
            .iter()
            .find(|p| p.replacement == "/quota")
            .expect("/quota is a candidate");
        let desc = super::super::commands::help_entries(super::super::commands::Surface::Repl)
            .into_iter()
            .find(|(l, _)| l.starts_with("/quota"))
            .map(|(_, d)| d)
            .expect("/quota is in the table");
        assert!(
            quote.display.contains(desc),
            "the listing carries the command's own words:\n  display: {:?}\n  desc:    {desc:?}",
            quote.display
        );
        assert_eq!(
            quote.replacement, "/quota",
            "only the name is inserted — never the prose"
        );
    }

    /// A word that is not a command gets NO candidates. The prompt is
    /// mostly prose, and Tab that completes English would be worse than
    /// Tab that does nothing.
    #[test]
    fn tab_leaves_ordinary_prose_alone() {
        let (_, pairs) = complete_at("what does the engine do");
        assert!(
            pairs.is_empty(),
            "prose must not be completed: {:?}",
            replacements(&pairs)
        );
    }

    /// An exact head is not offered as its own completion (it is already
    /// typed), but the ghost line still explains it.
    #[test]
    fn a_typed_command_is_explained_not_re_offered() {
        let (_, pairs) = complete_at("/status");
        assert!(
            !pairs.iter().any(|p| p.replacement == "/status"),
            "a fully-typed command is not a completion of itself"
        );
        // The ghost line, through the same helper the editor uses.
        let hint = hint_of("/status", 7).expect("a typed command gets a ghost line");
        assert!(
            hint.contains("session, model, lifecycle"),
            "the ghost line says what it does: {hint:?}"
        );
    }

    /// A partial word hints the REST of the name — the rest of the
    /// command, not its description, so the word is one keystroke away.
    #[test]
    fn a_partial_word_hints_the_rest_of_the_name() {
        let hint = hint_of("/stat", 5).expect("a partial command gets a ghost line");
        assert_eq!(hint, "us", "/stat + hint spells the command");
    }

    /// The completion reads the SAME table `/help` prints. A command in
    /// one but not the other is the drift this whole round is about.
    #[test]
    fn the_completion_and_help_come_from_one_table() {
        let h = helper();
        for (label, _) in
            super::super::commands::help_entries(super::super::commands::Surface::Repl)
        {
            let head = label.split_whitespace().next().unwrap_or(label);
            assert!(
                h.describe(head).is_some(),
                "help row '{label}' must be describable by the completer"
            );
        }
    }

    /// The word boundary is found, not assumed: completing the second
    /// word on a line must not replace the first.
    #[test]
    fn completion_only_touches_the_word_at_the_cursor() {
        let (start, _) = complete_at("/model /stat");
        assert_eq!(start, 7, "the second word starts after the space");
    }

    /// A multibyte line must not panic. `pos` is a byte offset and
    /// slicing mid-character is a panic in the one place a user is
    /// typing, so it is clamped and walked back to a boundary.
    #[test]
    fn a_multibyte_line_neither_panics_nor_swallows_the_command() {
        // "日本語 /sta" — 9 bytes of CJK, a space, then the command.
        let line = "日本語 /sta";
        let (start, pairs) = complete_at(line);
        assert_eq!(
            start,
            "日本語 ".len(),
            "the word starts after the multibyte prefix"
        );
        assert!(
            pairs.iter().any(|p| p.replacement == "/status"),
            "the command is still completed: {:?}",
            replacements(&pairs)
        );
        // And an offset landing mid-character must not panic either.
        let h = helper();
        let hist = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&hist);
        let mid = 1; // inside the first 日
        let _ = rustyline::completion::Completer::complete(&h, line, mid, &ctx)
            .expect("a mid-character cursor must not panic");
    }

    #[test]
    fn parse_input_routes_the_three_shapes() {
        // Plain speech is a prompt — the overwhelmingly common case.
        assert!(parse_input("hello there").is_prompt("hello there"));
        // Slash lines dispatch.
        assert!(parse_input("/help").is_slash("/help"));
        // Quit, with and without the delete flag.
        assert!(parse_input("/quit").is_quit(false));
        assert!(parse_input("/exit --delete").is_quit(true));
        // Case-insensitive: /QUIT is still a quit (a prompt that SAYS
        // /quit would be worse than exiting).
        assert!(parse_input("/QUIT").is_quit(false));
    }

    #[test]
    fn session_ids_are_timestamp_prefixed_and_unique() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b, "two sessions in the same second must differ");
        assert!(
            a.len() > 15 && a.contains('_'),
            "shape is <timestamp>_<uuid6>: {a}"
        );
    }

    #[test]
    fn status_line_reports_rounds_and_interruption() {
        let r = TurnResult {
            reply: String::new(),
            provider: "p".into(),
            model: "m".into(),
            interrupted: false,
            tool_rounds: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            cached_tokens: Some(7),
            reasoning_tokens: None,
        };
        let s = status_line(&r);
        assert!(s.contains("15 tok"), "total tokens: {s}");
        assert!(
            s.contains("3 uncached"),
            "the honest metric, prompt − cached: {s}"
        );
        assert!(!s.contains("tool round"), "zero rounds are not shown: {s}");
        assert!(!s.contains("interrupted"), "{s}");

        let r2 = TurnResult {
            tool_rounds: 3,
            interrupted: true,
            ..r
        };
        let s2 = status_line(&r2);
        assert!(s2.contains("3 tool round(s)"), "{s2}");
        assert!(s2.contains("interrupted"), "{s2}");
    }

    #[test]
    fn status_line_prints_no_uncached_number_when_cache_is_unreported() {
        // A provider that sends no cache field is NOT "0% cached" —
        // printing a number then would send somebody hunting a prefix
        // bug that does not exist.
        let r = TurnResult {
            reply: String::new(),
            provider: "p".into(),
            model: "m".into(),
            interrupted: false,
            tool_rounds: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            cached_tokens: None,
            reasoning_tokens: None,
        };
        let s = status_line(&r);
        assert!(!s.contains("uncached"), "{s}");
    }

    #[test]
    fn status_line_splits_answer_from_thinking_when_reported() {
        // A route that splits reasoning out of completion: the line must
        // name both, because "5 completion" reading as 5 answer tokens
        // is exactly the lie the separate count exists to expose.
        let r = TurnResult {
            reply: String::new(),
            provider: "p".into(),
            model: "m".into(),
            interrupted: false,
            tool_rounds: 0,
            prompt_tokens: 10,
            completion_tokens: 5,
            cached_tokens: None,
            reasoning_tokens: Some(2),
        };
        let s = status_line(&r);
        assert!(
            s.contains("3 answer · 2 thinking"),
            "the split names answer and thinking: {s}"
        );

        // None = the provider said nothing about reasoning, which is NOT
        // the same as zero — the line must stay silent, not print a 0.
        let silent = TurnResult {
            reasoning_tokens: None,
            ..r
        };
        assert!(
            !status_line(&silent).contains("thinking"),
            "an unknown reasoning count is not a 0: {}",
            status_line(&silent)
        );
    }
}
