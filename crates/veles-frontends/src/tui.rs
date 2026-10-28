//! TUI frontend — ratatui interactive surface (2026-08-10: real TUI).
//!
//! A chat TUI app, not a REPL: session sidebar, scrollable markdown
//! transcript, live streaming + tool progress, neon-synthwave palette.
//!
//! Layout:
//!   ┌─ ▲ VELES ─────────────────────────────────────────── 12:04 ─┐
//!   │ ┌ sessions ─────────┐ ┌ transcript … ─────────────────────┐ │
//!   │ │ ◆ today's session │ │ 12:03 you ▸ …                     │ │
//!   │ │   older one       │ │ 12:04 veles ▸ …                   │ │
//!   │ └───────────────────┘ │   └ markdown rendered             │ │
//!   │                       └───────────────────────────────────┘ │
//!   │ ┌ input ───────────────────────────────────────────────────┐ │
//!   │ │ ❯ █                                                     │ │
//!   │ └──────────────────────────────────────────────────────────┘ │
//!   │ ⟳ thinking… · route · 12→34 tok · session …  │ F1 help · …  │
//!   └──────────────────────────────────────────────────────────────┘
//!
//! Keys: Enter submit · Ctrl/Alt/⇧-Enter newline (multiline input;
//! the box scrolls vertically and horizontally with the caret) ·
//! Esc cancel turn / quit · ↑/↓ move between lines, history at the
//! edges · ←/→/Home/End edit (line-scoped) · Ctrl-U clear · Ctrl-W
//! kill word · Ctrl-K or Alt-x palette · Ctrl-/ undo · Ctrl-Z
//! suspend · Tab/⇧Tab switch session · Ctrl-N new · Ctrl-D delete
//! (empty input) · PgUp/PgDn + wheel scroll · Ctrl-End follow ·
//! F1 help.
//!
//! Palette: "16-bit neon impressionism / dark synthwave" — Hyper Light
//! Drifter pinks & cyans on deep violet-black, Mega Drive boldness,
//! Carpenter Brut contrast (gold/red sparingly).

use crate::{cur, handle, AgentHandle};
#[path = "mdtable.rs"]
mod mdtable;
use anyhow::Result;
use chrono::Local;
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Terminal,
};
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;
use veles_core::agent::Agent;
use veles_core::lockutil::{LockOk, RwLockOk};
use veles_core::providers::StreamEvent;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

// ── transcript model ────────────────────────────────────────────────
//
// `veles_core::transcript`, not a private `Vec<Msg>`. The private one was
// mutated from five places with a stream index captured once per turn,
// which is why tool calls collected at the bottom and reasoning was
// laid out one way live and another on reload. There is one chokepoint
// now and both paths go through it — see that module's header.
use veles_core::transcript::{Event as TEvent, Kind, Transcript};

/// A session in the sidebar (id, display label, message count).
struct SessionItem {
    id: String,
    label: String,
    count: i64,
}

/// What the composer did last, so Ctrl-/ can undo the run of keystrokes
/// that made a word rather than each character on its own.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LastEdit {
    /// One or more adjacent plain character inserts — undo treats them
    /// as one edit, the way a line editor does.
    CharRun,
    /// Anything else (delete, kill, yank, newline, paste).
    Other,
}

enum UiEvent {
    Key(event::KeyEvent),
    Mouse(event::MouseEvent),
    Tick,
    Delta(String),
    ReasoningDelta(String),
    /// (name, args-on-the-wire) — the header line, flattened.
    ToolStart(String, String),
    /// (name, result) — the result joins its header's block.
    ToolEnd(String, String),
    /// An engine note (guard fired, history compacted, failover, cache
    /// miss…). The transcript must show these — silent automation is
    /// the defect the notes machinery exists to prevent.
    Note(Box<veles_core::notes::EngineNote>),
    Ask(Box<veles_core::ask::AskRequest>),
    /// A bracketed paste arrived as one event (not as keystrokes).
    Paste(String),
    /// The agent's `focus_pane` tool routed in-process: raise/open the
    /// named pane in the TUI.
    FocusPane(String),
    /// Something is waiting in this session's queue — mail another
    /// session sent (`veles_core::mail`), nudged over this process's
    /// socket. Idle → run it now; mid-turn → the boundary drain has it.
    QueueNudge,
    TurnEnd {
        text: String,
        route: String,
        model: String,
        ok: bool,
        interrupted: bool,
        prompt_tokens: u64,
        completion_tokens: u64,
        /// None = provider reports no cache field (unknown, not zero).
        cached_tokens: Option<u64>,
        /// None = route did not split reasoning out (unknown, not zero).
        reasoning_tokens: Option<u64>,
    },
}

/// The TUI's session, as one this process holds open (session mail).
/// One session at a time — whichever the TUI shows now.
struct TuiPresence {
    agent: AgentHandle,
    tx: Mutex<mpsc::Sender<UiEvent>>,
}

impl veles_core::presence::PresenceSource for TuiPresence {
    fn open_sessions(&self) -> Vec<veles_core::presence::OpenSession> {
        vec![veles_core::presence::OpenSession {
            session_id: cur(&self.agent).session_id.clone(),
            surface: "tui".into(),
        }]
    }

    fn nudge(&self, session_id: &str) -> bool {
        if cur(&self.agent).session_id != session_id {
            return false;
        }
        let _ = self.tx.lock_ok().send(UiEvent::QueueNudge);
        true
    }
}

pub fn run(resume: bool, ov: &veles_core::agent::ModelOverrides) -> Result<()> {
    let boot = crate::cli::boot(resume)?;
    // The flags reach the TUI now. They used to stop at main.rs.
    for w in boot.agent.apply_overrides(ov) {
        eprintln!("warning: {w}");
    }

    // ── shared state ─────────────────────────────────────────────────
    let agent: AgentHandle = handle(boot.agent);
    let history: Arc<Mutex<Vec<veles_core::providers::ChatMessage>>> =
        Arc::new(Mutex::new(Vec::new()));
    let session_id = cur(&agent).session_id.clone();

    // Ask channel: the agent's questions arrive here and are forwarded
    // to the UI loop (the turn thread itself must never block the UI).
    // The sender is shared: the initial agent AND every session-switched
    // agent all ask through the same pipe.
    let (ask_tx, ask_rx) = mpsc::channel::<veles_core::ask::AskRequest>();
    cur(&agent).set_asker(Arc::new(veles_core::ask::ChannelAsker {
        tx: ask_tx.clone(),
    }));

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Bracketed paste: without it a multi-line paste arrives as
    // keystrokes, so the first newline SUBMITS and the rest of the
    // snippet scatters into the next prompt.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // ── UI state ─────────────────────────────────────────────────────
    let mut st = UiState::new(session_id.clone());
    st.agent_snapshot = Some(agent.clone());
    st.ask_tx = Some(ask_tx);
    reload_session(&mut st, &agent, &history, &session_id);

    // Terminal events (keys + mouse) arrive on a thread; the tick
    // thread drives the spinner.
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>();
    // The agent's `open_preview`/`focus_pane` tools route in-process
    // here instead of erroring "desktop not running": a URL opens in the
    // browser, a file says where it is, and focus_pane lets the agent
    // raise the panes this surface actually has.
    veles_core::desktop_tools::set_local_pane_target({
        let ui_tx = ui_tx.clone();
        move |name: &str, args: &serde_json::Value| -> Option<String> {
            match name {
                "open_preview" => {
                    let target = args["url"].as_str().unwrap_or_default().trim();
                    if target.is_empty() {
                        return Some("error: url is required".into());
                    }
                    if target.starts_with("http://") || target.starts_with("https://") {
                        match open_url(target) {
                            Ok(()) => Some(format!(
                                "opened {target} in the browser — the TUI has no preview pane"
                            )),
                            Err(e) => Some(format!("could not open {target}: {e}")),
                        }
                    } else {
                        Some(format!(
                            "preview: {target} is on disk — the TUI has no preview pane \
                             (open it in your editor, or read it)"
                        ))
                    }
                }
                "focus_pane" => {
                    let pane = args["pane"].as_str().unwrap_or_default().to_string();
                    if pane.is_empty() {
                        return Some("error: pane is required".into());
                    }
                    let _ = ui_tx.send(UiEvent::FocusPane(pane.clone()));
                    Some(veles_core::i18n::lookup(
                        "focused pane: %{pane}",
                        &[("pane", pane)],
                    ))
                }
                _ => None,
            }
        }
    });
    // Session mail (veles_core::mail): this process's socket, and its
    // session as one siblings can list and nudge. The session is read
    // at ask time, so `/new` and `/resume` move the mailbox with it.
    veles_core::runsock::serve("tui");
    let presence_key = veles_core::presence::register(Arc::new(TuiPresence {
        agent: agent.clone(),
        tx: Mutex::new(ui_tx.clone()),
    }));
    // Mail that waited while nobody had this session open runs now.
    let _ = ui_tx.send(UiEvent::QueueNudge);
    let ui_events_tx = ui_tx.clone();
    let turn_tx = ui_tx.clone();
    let tx2 = ui_tx.clone();
    std::thread::spawn(move || loop {
        match event::read() {
            Ok(Event::Key(ev)) => {
                if ui_events_tx.send(UiEvent::Key(ev)).is_err() {
                    break;
                }
            }
            Ok(Event::Mouse(ev)) => {
                if ui_events_tx.send(UiEvent::Mouse(ev)).is_err() {
                    break;
                }
            }
            Ok(Event::Paste(text)) => {
                if ui_events_tx.send(UiEvent::Paste(text)).is_err() {
                    break;
                }
            }
            _ => {}
        }
    });
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(120));
        if tx2.send(UiEvent::Tick).is_err() {
            break;
        }
    });

    // Questions → UI loop.
    std::thread::spawn(move || {
        while let Ok(req) = ask_rx.recv() {
            if turn_tx.send(UiEvent::Ask(Box::new(req))).is_err() {
                break;
            }
        }
    });

    // The whole event loop is a closure so ANY `?` (draw errors, quit)
    // returns to the cleanup below instead of escaping `run` with the
    // terminal still in raw mode.
    let result: Result<()> = (|| -> Result<()> {
        draw(&mut terminal, &mut st)?;
        // The queue row THIS loop's live turn took, if any (composer
        // turns own none). Released at the next TurnEnd BEFORE the next
        // take — a completed row left `running` reads as a crashed
        // taker's corpse and would re-run forever (2026-09-08 flood).
        let mut cur_seq: Option<i64> = None;
        loop {
            match ui_rx.recv() {
                Ok(UiEvent::Tick) => {
                    if st.busy {
                        st.spinner = (st.spinner + 1) % SPINNER.len();
                    }
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::Paste(text)) => {
                    // One event for the whole clipboard, newlines and
                    // all — nothing here can accidentally submit.
                    // CRs normalize to \n so a Windows-ish paste
                    // renders as lines, not '·' walls (scrub_controls
                    // maps stray control bytes visibly).
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    if st.pending_ask.is_some() {
                        st.ask_input.push_str(&text);
                    } else {
                        st.input.insert_str(st.caret, &text);
                        st.caret += text.len();
                    }
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::Delta(t)) => {
                    st.append_delta(&t);
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::FocusPane(pane)) => {
                    match pane.as_str() {
                        "files" => {
                            st.picker = Some(files_picker("."));
                        }
                        "review" => {
                            st.picker = Some(diff_picker());
                        }
                        "log" => {
                            st.show_log = true;
                            st.log_scroll = 0;
                            st.push_msg_status(veles_core::i18n::lookup("log pane shown", &[]));
                        }
                        "chat" => {
                            st.picker = None;
                            st.detail = None;
                            st.palette = None;
                        }
                        // Panes this surface does not have map to an
                        // honest note rather than a pretend window.
                        other => st.push_msg_status(veles_core::i18n::lookup(
                            "the TUI has no '%{other}' pane — /help lists what it does have",
                            &[("other", other.to_string())],
                        )),
                    }
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::ReasoningDelta(t)) => {
                    st.append_reasoning(&t);
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::ToolStart(name, args)) => {
                    st.tool_running = Some(name.clone());
                    st.emit(TEvent::ToolStart {
                        name,
                        args,
                        call_id: String::new(),
                    });
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::ToolEnd(name, result)) => {
                    st.tool_running = None;
                    st.finish_tool(&name, result);
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::Note(n)) => {
                    // The block model keeps the note's kind, summary,
                    // detail and toggle as structure (render_msg picks
                    // the glyph) — the old path flattened all of that
                    // into one grey line.
                    st.emit(TEvent::Note(veles_core::transcript::note_from_engine(&n)));
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::Ask(req)) => {
                    st.pending_ask = Some(*req);
                    st.ask_input.clear();
                    st.ask_scroll = 0;
                    st.ask_sel = 0;
                    st.ask_toggles.clear();
                    st.ask_other.clear();
                    st.ask_note_mode = false;
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::TurnEnd {
                    text,
                    route,
                    model,
                    ok,
                    interrupted,
                    prompt_tokens,
                    completion_tokens,
                    cached_tokens,
                    reasoning_tokens,
                }) => {
                    st.end_turn(
                        &text,
                        &route,
                        &model,
                        ok,
                        interrupted,
                        prompt_tokens,
                        completion_tokens,
                        cached_tokens,
                        reasoning_tokens,
                        &agent,
                    );
                    // R4.6: the turn boundary drains the shared queue —
                    // text parked from ANY surface while this turn ran
                    // (this composer included) rolls on as the next
                    // turn without another Enter. launch_turn pushes
                    // the user block, so the queued text becomes
                    // visible exactly when it actually runs.
                    let next = {
                        let a = cur(&agent);
                        let sid = a.session_id.clone();
                        // Retire OUR completed row before the take (see
                        // cur_seq above): left `running`, the next take
                        // reads it as a crashed taker and re-runs the
                        // same message forever (2026-09-08 flood).
                        if let Some(seq) = cur_seq.take() {
                            let _ = a.store.lock_ok().queue_release(&sid, seq);
                        }
                        let item = a
                            .store
                            .lock_ok()
                            .queue_take_next(&sid)
                            .map_err(|e| {
                                tracing::warn!("queue: taking {sid}'s next row failed: {e:#}")
                            })
                            .ok()
                            .flatten();
                        drop(a);
                        item
                    };
                    if let Some(item) = next {
                        cur_seq = Some(item.seq);
                        launch_queued(&mut st, &agent, &history, &ui_tx, item);
                    }
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::QueueNudge) => {
                    // Mid-turn the boundary drain above takes it; a
                    // modal or a half-typed prompt does not stop it —
                    // mail runs like a queued message, the composer
                    // keeps its text.
                    if !st.busy {
                        let a = cur(&agent);
                        let sid = a.session_id.clone();
                        let item = a
                            .store
                            .lock_ok()
                            .queue_take_next(&sid)
                            .map_err(|e| {
                                tracing::warn!("queue: taking {sid}'s next row failed: {e:#}")
                            })
                            .ok()
                            .flatten();
                        drop(a);
                        if let Some(item) = item {
                            cur_seq = Some(item.seq);
                            launch_queued(&mut st, &agent, &history, &ui_tx, item);
                            draw(&mut terminal, &mut st)?;
                        }
                    }
                }
                Ok(UiEvent::Key(key)) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    // A modal is on screen: keys answer it, nothing else.
                    if st.confirm_delete.is_some() {
                        match key.code {
                            KeyCode::Enter => {
                                let sid = st.confirm_delete.take().unwrap();
                                if let Err(e) = delete_session(&agent, &sid) {
                                    st.push_msg_status(veles_core::i18n::lookup(
                                        "delete failed: %{e}",
                                        &[("e", format!("{e:#}").to_string())],
                                    ));
                                }
                                refresh_sessions(&mut st, &agent);
                            }
                            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                                st.confirm_delete = None;
                            }
                            _ => {}
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    if let Some(req) = st.pending_ask.clone() {
                        // An approval's note line (Tab): while it has the
                        // keys, characters are the note, Enter sends the
                        // note ALONE (core reads that as declined / keep
                        // planning, with the note), Tab or Esc hand the
                        // keys back to the choices.
                        if takes_note(&req) {
                            if st.ask_note_mode {
                                match key.code {
                                    KeyCode::Tab | KeyCode::Esc => st.ask_note_mode = false,
                                    KeyCode::Enter => {
                                        let note = st.ask_input.trim().to_string();
                                        if !note.is_empty() {
                                            veles_core::ask::resolve(
                                                &req.id,
                                                veles_core::ask::AskAnswer::text(&note),
                                            );
                                            st.pending_ask = None;
                                            st.ask_note_mode = false;
                                        }
                                    }
                                    KeyCode::Backspace => {
                                        st.ask_input.pop();
                                    }
                                    KeyCode::Char(c) => st.ask_input.push(c),
                                    _ => {}
                                }
                                draw(&mut terminal, &mut st)?;
                                continue;
                            }
                            if key.code == KeyCode::Tab {
                                st.ask_note_mode = true;
                                draw(&mut terminal, &mut st)?;
                                continue;
                            }
                        }
                        match ask_key(&req, st.ask_sel, &st.ask_toggles, &key.code) {
                            AskKeyEffect::Answer(a) => {
                                let a = with_note(&req, a, &st.ask_input);
                                veles_core::ask::resolve(&req.id, a);
                                st.pending_ask = None;
                                st.ask_note_mode = false;
                            }
                            AskKeyEffect::SetSel(n) => st.ask_sel = n,
                            AskKeyEffect::Toggle { addr, radio } => {
                                if radio {
                                    let q = addr.split(':').next().unwrap_or("").to_string();
                                    st.ask_toggles.retain(|a| !a.starts_with(&format!("{q}:")));
                                }
                                if !st.ask_toggles.remove(&addr) {
                                    st.ask_toggles.insert(addr);
                                }
                            }
                            AskKeyEffect::Type(c) => match ask_typing_target(&req, st.ask_sel) {
                                TypingTarget::Other(qk) => {
                                    st.ask_other.entry(qk).or_default().push(c);
                                }
                                TypingTarget::Plain => st.ask_input.push(c),
                                TypingTarget::Nowhere => {}
                            },
                            AskKeyEffect::Backspace => match ask_typing_target(&req, st.ask_sel) {
                                TypingTarget::Other(qk) => {
                                    if let Some(s) = st.ask_other.get_mut(&qk) {
                                        s.pop();
                                    }
                                }
                                TypingTarget::Plain => {
                                    st.ask_input.pop();
                                }
                                TypingTarget::Nowhere => {}
                            },
                            AskKeyEffect::Commit => {
                                if let Some(a) = commit_ask(
                                    &req,
                                    st.ask_sel,
                                    &st.ask_toggles,
                                    &st.ask_other,
                                    &st.ask_input,
                                ) {
                                    veles_core::ask::resolve(&req.id, a);
                                    st.pending_ask = None;
                                }
                            }
                            AskKeyEffect::ScrollUp => {
                                st.ask_scroll = st.ask_scroll.saturating_sub(1)
                            }
                            AskKeyEffect::ScrollDown => st.ask_scroll += 1,
                            AskKeyEffect::None => {}
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    if st.show_help {
                        match key.code {
                            KeyCode::Esc | KeyCode::F(1) => st.show_help = false,
                            _ => {}
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    // Detail pager (a View picker row): scroll, Esc/q closes.
                    if st.detail.is_some() {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => st.detail = None,
                            KeyCode::Up | KeyCode::Char('k') => {
                                st.detail.as_mut().unwrap().scroll =
                                    st.detail.as_ref().unwrap().scroll.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                st.detail.as_mut().unwrap().scroll += 1;
                            }
                            KeyCode::PageUp => {
                                st.detail.as_mut().unwrap().scroll =
                                    st.detail.as_ref().unwrap().scroll.saturating_sub(14);
                            }
                            KeyCode::PageDown => {
                                st.detail.as_mut().unwrap().scroll += 14;
                            }
                            KeyCode::Home => st.detail.as_mut().unwrap().scroll = 0,
                            _ => {}
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    // Find-in-transcript (Ctrl-F): type to filter,
                    // Enter/n next, ⇧Enter/N previous, Esc closes.
                    if st.search.is_some() {
                        let mut s = st.search.take().unwrap();
                        let mut close = false;
                        match key.code {
                            KeyCode::Esc => close = true,
                            KeyCode::Backspace => {
                                s.query.pop();
                                s.recompute(&st.transcript);
                                st.scroll_to_msg = s.current();
                            }
                            KeyCode::Enter | KeyCode::Down => {
                                st.scroll_to_msg = s.step(true);
                            }
                            KeyCode::Up => {
                                st.scroll_to_msg = s.step(false);
                            }
                            KeyCode::Char(c) => {
                                s.query.push(c);
                                s.recompute(&st.transcript);
                                st.scroll_to_msg = s.current();
                            }
                            _ => {}
                        }
                        if !close {
                            st.search = Some(s);
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    // Link picker (Ctrl-O): keyboard route to the same
                    // URLs the mouse can click.
                    if let Some(sel) = st.link_picker {
                        let n = st.all_links.len();
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('q') => st.link_picker = None,
                            KeyCode::Up => {
                                st.link_picker = Some(sel.saturating_sub(1));
                            }
                            KeyCode::Down => {
                                st.link_picker = Some((sel + 1).min(n.saturating_sub(1)));
                            }
                            KeyCode::Enter => {
                                if let Some(url) = st.all_links.get(sel).cloned() {
                                    match open_url(&url) {
                                        Ok(()) => st.push_msg_status(veles_core::i18n::lookup(
                                            "opened %{url}",
                                            &[("url", url.to_string())],
                                        )),
                                        Err(e) => st.push_msg_status(veles_core::i18n::lookup(
                                            "could not open %{url}: %{e}",
                                            &[("url", url.to_string()), ("e", e.to_string())],
                                        )),
                                    }
                                }
                                st.link_picker = None;
                            }
                            KeyCode::Char(c) if c.is_ascii_digit() => {
                                let idx = c.to_digit(10).unwrap_or(0) as usize;
                                if idx >= 1 && idx <= n {
                                    if let Some(url) = st.all_links.get(idx - 1).cloned() {
                                        match open_url(&url) {
                                            Ok(()) => st.push_msg_status(veles_core::i18n::lookup(
                                                "opened %{url}",
                                                &[("url", url.to_string())],
                                            )),
                                            Err(e) => st.push_msg_status(veles_core::i18n::lookup(
                                                "could not open %{url}: %{e}",
                                                &[("url", url.to_string()), ("e", e.to_string())],
                                            )),
                                        }
                                    }
                                    st.link_picker = None;
                                }
                            }
                            _ => {}
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    // M-x palette: type to filter, up/down select, Enter run,
                    // Esc closes. Works in every keymap.
                    if st.palette.is_some() {
                        let mut close = false;
                        let mut run: Option<PaletteAction> = None;
                        match key.code {
                            KeyCode::Esc => close = true,
                            KeyCode::Backspace => {
                                st.palette.as_mut().unwrap().query.pop();
                                st.palette.as_mut().unwrap().sel = 0;
                            }
                            KeyCode::Enter => {
                                let entries = filtered_palette(&st);
                                if let Some(entry) = entries.get(st.palette.as_ref().unwrap().sel) {
                                    run = Some(entry.action.clone());
                                }
                                close = true;
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                let n = filtered_palette(&st).len();
                                let p = st.palette.as_mut().unwrap();
                                p.sel = (p.sel + 1).min(n.saturating_sub(1));
                            }
                            KeyCode::Up | KeyCode::Char('k') => {
                                let p = st.palette.as_mut().unwrap();
                                p.sel = p.sel.saturating_sub(1);
                            }
                            KeyCode::Char(c) => {
                                let p = st.palette.as_mut().unwrap();
                                p.query.push(c);
                                p.sel = 0;
                            }
                            _ => {}
                        }
                        if let Some(action) = run {
                            run_palette_action(&mut st, &agent, &history, &ui_tx, action);
                        }
                        if close {
                            st.palette = None;
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    // Modal picker (the /model selector). Same shape as
                    // the link picker: ↑/↓ move, Enter runs, Esc closes.
                    if let Some(sel) = st.picker.as_ref().map(|p| p.sel) {
                        let n = st.picker.as_ref().map(|p| p.rows.len()).unwrap_or(0);
                        let mut close = false;
                        let mut run = false;
                        match key.code {
                            KeyCode::Esc => close = true,
                            KeyCode::Up | KeyCode::Char('k') => {
                                st.picker.as_mut().unwrap().sel = sel.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Char('j') => {
                                st.picker.as_mut().unwrap().sel =
                                    (sel + 1).min(n.saturating_sub(1));
                            }
                            KeyCode::Enter => {
                                run = true;
                                close = true;
                            }
                            _ => {}
                        }
                        if run {
                            run_picker_selection(&mut st, &agent, &history, sel);
                        } else if close {
                            st.picker = None;
                        }
                        draw(&mut terminal, &mut st)?;
                        continue;
                    }
                    if handle_key(&mut st, &agent, &history, &ui_tx, key) || st.quit {
                        break Ok(());
                    }
                    draw(&mut terminal, &mut st)?;
                }
                Ok(UiEvent::Mouse(ev)) => {
                    handle_mouse(&mut st, &agent, &history, &ui_tx, ev);
                    draw(&mut terminal, &mut st)?;
                }
                Err(_) => break Ok(()),
            }
        }
    })();

    veles_core::presence::unregister(presence_key);
    veles_core::runsock::shutdown();
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;
    result
}

// ── UI state & logic ────────────────────────────────────────────────

/// One message's finished rendering, kept between frames.
///
/// Before this existed, every frame re-parsed the markdown of the WHOLE
/// transcript (plus, since links landed, two regex scans per line),
/// exploded it into one Span per character, and then cloned the entire
/// result for the Paragraph. The tick thread redraws 8×/s regardless of
/// input, so a long session pinned a core at 100% while idle — and
/// scrolling, which changes nothing but the offset, paid the full cost
/// per wheel event.
#[derive(Clone)]
struct MsgRender {
    kind: Kind,
    /// Cheap staleness key: how much source text the block renders
    /// from (`src_len`) — deltas grow it, `finish_reply`/tool results
    /// replace it wholesale behind an explicit invalidate.
    len: usize,
    /// Reasoning renders collapsed or expanded; the key must see the
    /// flip even though the source text did not change.
    collapsed: bool,
    lines: Vec<Line<'static>>,
    /// (row within this message, col start, col end, url)
    links: Vec<(usize, usize, usize, String)>,
}

#[derive(Default)]
struct RenderCache {
    width: u16,
    theme: String,
    raw: bool,
    msgs: Vec<Option<MsgRender>>,
}

impl RenderCache {
    fn invalidate_all(&mut self) {
        self.msgs.clear();
    }
    fn invalidate(&mut self, idx: usize) {
        if let Some(slot) = self.msgs.get_mut(idx) {
            *slot = None;
        }
    }
}

/// Find-in-transcript state. Matching runs over `Msg.text` — the source
/// of truth — rather than over rendered rows, which are frame-local and
/// change with every resize and theme switch.
#[derive(Default, Clone)]
struct SearchState {
    query: String,
    /// Indices into `UiState.transcript`, in order.
    hits: Vec<usize>,
    /// Which hit is current.
    at: usize,
}

impl SearchState {
    fn recompute(&mut self, transcript: &Transcript) {
        let q = self.query.to_lowercase();
        self.hits = if q.is_empty() {
            Vec::new()
        } else {
            transcript
                .blocks
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    let hay = match &m.tool {
                        // A tool block's searchable text is its name,
                        // arguments and result — `body` is empty.
                        Some(tool) => format!(
                            "{} {} {}",
                            tool.name,
                            tool.args,
                            tool.result.as_deref().unwrap_or("")
                        ),
                        None => m.body.clone(),
                    };
                    hay.to_lowercase().contains(&q)
                })
                .map(|(i, _)| i)
                .collect()
        };
        if self.at >= self.hits.len() {
            self.at = 0;
        }
    }
    fn current(&self) -> Option<usize> {
        self.hits.get(self.at).copied()
    }
    fn step(&mut self, forward: bool) -> Option<usize> {
        if self.hits.is_empty() {
            return None;
        }
        self.at = if forward {
            (self.at + 1) % self.hits.len()
        } else {
            (self.at + self.hits.len() - 1) % self.hits.len()
        };
        self.current()
    }
}

struct UiState {
    /// Current session id (the agent's session).
    session: String,
    transcript: Transcript,
    sessions: Vec<SessionItem>,
    sel: usize,
    input: String,
    caret: usize, // byte index
    /// Composer scroll: the first VISIBLE line of a multiline input
    /// (the box grows with content up to half the screen, then this
    /// scrolls; the caret row is always kept visible).
    input_vscroll: u16,
    hist: Vec<String>,
    hist_idx: Option<usize>,
    busy: bool,
    /// `/quit` was typed — the event loop breaks once the handler
    /// returns (handle_key's bool path is only for keys).
    quit: bool,
    /// Plan mode is on for this session — the status bar says so.
    plan_mode: bool,
    route: String,
    model: String,
    /// The upstream plan's reading, as `(label, percent)` — `None` until
    /// a reading is available, which renders as nothing rather than as
    /// "no plan". Read from `tokenplan`'s in-process cache (no I/O) on
    /// the same refresh points the token counters use, because every
    /// other surface shows it and "all backend state visible in all
    /// frontends" is the standing rule.
    ///
    /// The LABEL is part of the pair on purpose (USER 2026-09-20: "the
    /// existing usage is confusing, it conflates the personal and team
    /// alibaba tokens … clearly label the plans separately"). A bare
    /// "plan 59%" names neither the pool nor the bill, and four
    /// providers on this box draw on four disjoint pools.
    plan_reading: Option<(String, f64)>,
    prompt_tok: u64,
    completion_tok: u64,
    /// Session cache totals. `cache_seen` stays false until some turn
    /// reports a cache field — until then the status bar prints no
    /// uncached number (unknown is not zero).
    cached_tok: u64,
    cache_seen: bool,
    /// Session reasoning totals. None until a route reports it — the
    /// footer shows the answer/thinking split only once it is known.
    reasoning_tok: Option<u64>,
    tool_running: Option<String>,
    spinner: usize,
    /// Lines scrolled back from the bottom (0 = follow the stream).
    scrollback: u16,
    pending_ask: Option<veles_core::ask::AskRequest>,
    ask_input: String,
    /// Scroll position of the ask popup's detail (plan-mode approvals
    /// carry whole plans; see draw_ask).
    ask_scroll: u16,
    /// Multi-select/multi-question asks: the highlighted row in the
    /// choice list (Up/Down move it, Space toggles, Enter commits —
    /// see the ask-mode key arm). A plain single-choice ask never
    /// reads it: the first matching key still answers directly.
    ask_sel: usize,
    /// The set of toggled choice keys, built as "q1:c1" addresses so
    /// one set serves multi-question asks too.
    ask_toggles: std::collections::BTreeSet<String>,
    /// Per-question "Other" text, keyed by question key — the free-
    /// text-with-choices affordance the desktop's amend entry has
    /// always had and the TUI lacked (2026-09-26 parity audit).
    ask_other: std::collections::BTreeMap<String, String>,
    /// An approval's NOTE line has the keys (Tab toggles): typed
    /// characters go to `ask_input` as the note instead of answering,
    /// and the choice key picked afterwards carries the note (USER
    /// 2026-10-10: notes on asks were dropped on every surface).
    ask_note_mode: bool,
    confirm_delete: Option<String>,
    show_help: bool,
    /// Engine log pane (F2). Logs are captured into `veles_core::logbuf`
    /// rather than printed — stdout belongs to ratatui here.
    show_log: bool,
    /// Lines scrolled back in the log pane (0 = tail).
    log_scroll: u16,
    cancel: Option<veles_core::providers::CancelToken>,
    /// Ask channel shared with the agent (re-wired on session switch).
    ask_tx: Option<mpsc::Sender<veles_core::ask::AskRequest>>,
    /// Areas from the last draw pass — mouse hit-testing.
    sidebar_area: Option<Rect>,
    input_area: Option<Rect>,
    /// Clickable link regions on screen, rebuilt every draw:
    /// (row, col_start, col_end, url).
    link_hits: Vec<(u16, u16, u16, String)>,
    /// Rows occupied by reasoning headers (for click-to-expand).
    reasoning_header_rows: Vec<(u16, usize)>, // (screen_row, transcript_idx)
    /// Every link in the transcript, for the Ctrl-O picker.
    all_links: Vec<String>,
    link_picker: Option<usize>,
    /// Rendered-transcript cache (see `MsgRender`).
    cache: RenderCache,
    /// Find-in-transcript (Ctrl-F): query, matching message indices,
    /// and which match is selected.
    search: Option<SearchState>,
    /// First row of each message from the last draw — search jumps need
    /// the wrapped layout, which only exists inside draw_chat.
    msg_rows: Vec<usize>,
    /// Message index to scroll to on the next draw.
    scroll_to_msg: Option<usize>,
    /// Render replies as raw markdown source instead (F3).
    raw_view: bool,
    /// Live markdown preview of the composer (F4).
    show_preview: bool,
    /// Active palette (synthwave default; /theme switches it live).
    theme: veles_core::theme::Theme,
    /// Active keyboard scheme (normie/emacs/vi); /bindings switches it.
    keymap: veles_core::keymap::Keymap,
    /// vi mode: insert (true) vs normal (false).
    vi_insert: bool,
    /// Kill-ring for emacs C-k / C-y (and vi's x/dd reuse it).
    yank: String,
    /// Composer undo ring (Ctrl-/): (input, caret) snapshots, newest
    /// last. Bounded so a long session cannot grow it without limit.
    undo: Vec<(String, usize)>,
    /// What the composer did last (drives undo coalescing).
    last_edit: LastEdit,
    /// M-x command palette (the "do anything" entry point, all modes).
    palette: Option<PaletteState>,
    /// Modal list picker ("editors not dumps"): ↑/↓ move, Enter runs the
    /// chosen row's action, Esc closes.
    picker: Option<Picker>,
    /// Scrollable read-only pager opened by a `View` picker row.
    detail: Option<DetailView>,
    /// The handle local slash commands read (see `AgentHandle`).
    agent_snapshot: Option<AgentHandle>,
}

impl UiState {
    fn new(session: String) -> Self {
        Self {
            session,
            transcript: Transcript::new(),
            sessions: Vec::new(),
            sel: 0,
            input: String::new(),
            input_vscroll: 0,
            caret: 0,
            hist: Vec::new(),
            hist_idx: None,
            busy: false,
            quit: false,
            plan_mode: false,
            route: String::new(),
            model: String::new(),
            plan_reading: None,
            prompt_tok: 0,
            completion_tok: 0,
            cached_tok: 0,
            cache_seen: false,
            reasoning_tok: None,
            tool_running: None,
            spinner: 0,
            scrollback: 0,
            pending_ask: None,
            ask_input: String::new(),
            ask_scroll: 0,
            ask_sel: 0,
            ask_toggles: std::collections::BTreeSet::new(),
            ask_other: std::collections::BTreeMap::new(),
            ask_note_mode: false,
            confirm_delete: None,
            show_help: false,
            show_log: false,
            log_scroll: 0,
            link_hits: Vec::new(),
            reasoning_header_rows: Vec::new(),
            all_links: Vec::new(),
            link_picker: None,
            cache: RenderCache::default(),
            search: None,
            msg_rows: Vec::new(),
            scroll_to_msg: None,
            raw_view: false,
            show_preview: false,
            cancel: None,
            ask_tx: None,
            sidebar_area: None,
            input_area: None,
            theme: veles_core::theme::default_theme(),
            keymap: veles_core::keymap::default_keymap(),
            vi_insert: true,
            yank: String::new(),
            undo: Vec::new(),
            last_edit: LastEdit::Other,
            palette: None,
            picker: None,
            detail: None,
            agent_snapshot: None,
        }
    }

    /// Everything this surface adds to the transcript goes through the
    /// shared chokepoint; `scrollback = 0` is the only thing left that
    /// is this surface's business (R6: the live edge stays live).
    fn emit(&mut self, ev: TEvent) {
        self.scrollback = 0;
        self.transcript.push(ev);
        self.cache.invalidate_all();
    }

    fn push_msg_status(&mut self, text: String) {
        self.emit(TEvent::Status(text));
    }

    /// A chunk of prose. `Transcript::push` decides whether it extends
    /// the open reply or opens a new one — which is the whole fix for
    /// "round two's text landed above the tool it followed".
    fn append_delta(&mut self, t: &str) {
        self.scrollback = 0;
        let before = self.transcript.len();
        self.transcript.push(TEvent::Delta(t.to_string()));
        if self.transcript.len() != before {
            self.cache.invalidate_all();
        } else if let Some(i) = self.transcript.len().checked_sub(1) {
            self.cache.invalidate(i);
        }
    }

    /// A chunk of reasoning. It APPENDS like everything else now — the
    /// old version INSERTED above the live reply and shifted every
    /// later index, which is why reasoning sat above the reply live and
    /// below it on reload.
    fn append_reasoning(&mut self, t: &str) {
        self.scrollback = 0;
        let before = self.transcript.len();
        self.transcript.push(TEvent::ReasoningDelta(t.to_string()));
        if self.transcript.len() != before {
            self.cache.invalidate_all();
        } else if let Some(i) = self.transcript.len().checked_sub(1) {
            self.cache.invalidate(i);
        }
    }

    fn finish_tool(&mut self, name: &str, result: String) {
        self.emit(TEvent::ToolEnd {
            name: name.to_string(),
            result,
            call_id: String::new(),
        });
    }

    // One turn's worth of result fields, all distinct scalars from the
    // same event — bundling them into a struct would just move the
    // argument list somewhere else.
    #[allow(clippy::too_many_arguments)]
    fn end_turn(
        &mut self,
        text: &str,
        route: &str,
        model: &str,
        ok: bool,
        interrupted: bool,
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: Option<u64>,
        reasoning_tokens: Option<u64>,
        agent: &AgentHandle,
    ) {
        self.busy = false;
        self.tool_running = None;
        self.cancel = None;
        self.pending_ask = None;
        // The canonical text replaces the streamed one, everything
        // closes, reasoning collapses but STAYS (USER decision — it
        // used to be deleted here and reappear elsewhere on reload).
        let mut final_text = text.to_string();
        if interrupted {
            final_text.push_str("\n\n[interrupted]");
        }
        self.transcript.finish_reply(&final_text);
        // The tail reply's body was replaced wholesale; the length key
        // cannot see that.
        self.cache.invalidate_all();
        self.route = route.to_string();
        self.model = model.to_string();
        // The plan field, refreshed on the same beat the route and the
        // token counts are: it is an in-process cache read, and a turn
        // end is exactly when the number it shows has just moved.
        self.plan_reading = {
            let a = agent.lock_ok();
            plan_reading_for(&a.cfg, route)
        };
        self.prompt_tok += prompt_tokens;
        self.completion_tok += completion_tokens;
        if let Some(c) = cached_tokens {
            self.cached_tok += c;
            self.cache_seen = true;
        }
        // Reasoning accumulates only when a route reports it; None is
        // "this route folds thinking into completion", not zero.
        if let Some(r) = reasoning_tokens {
            self.reasoning_tok = Some(self.reasoning_tok.unwrap_or(0) + r);
        }
        self.scrollback = 0;
        if !ok {
            // TurnResult errors arrive as Err() → the error text is
            // already in `text` as "[error] …"; nothing extra needed.
        }
        // Plan mode, re-read from the store at the end of every turn:
        // `/plan` (this surface's own arm) and `present_plan` both
        // write the meta row, so a cached copy would be wrong the
        // moment either lands. Cheap — one meta row.
        self.plan_mode = agent
            .lock()
            .ok()
            .and_then(|a| {
                let store = a.store.clone();
                let sid = a.session_id.clone();
                store
                    .lock()
                    .ok()
                    .map(|s| veles_core::plan::mode_for(&s, &sid) == veles_core::plan::Mode::Plan)
            })
            .unwrap_or(false);
        // Open-todo count keeps the plan visible between turns.
        let open = agent
            .lock()
            .ok()
            .and_then(|a| {
                let store = a.store.clone();
                let sid = a.session_id.clone();
                store.lock().ok().and_then(|s| s.todos_list(&sid).ok())
            })
            .map(|items| {
                items
                    .iter()
                    .filter(|i| i.status == "pending" || i.status == "in_progress")
                    .count()
            })
            .unwrap_or(0);
        if open > 0 {
            self.push_msg_status(veles_core::i18n::lookup(
                "%{open} open todo — /todos",
                &[("open", open.to_string())],
            ));
        }
    }

    /// Switch the agent + transcript to another session.
    fn switch_to(
        &mut self,
        agent: &AgentHandle,
        history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
        sid: &str,
    ) {
        if self.busy {
            return;
        }
        self.cancel = None;
        self.pending_ask = None;
        self.confirm_delete = None;
        self.route.clear();
        self.model.clear();
        self.tool_running = None;
        self.scrollback = 0;

        // Rebuild the agent bound to the new session (registry is
        // session-scoped: todo/clarify). Reuse cfg/hooks/store.
        let (cfg, hooks, store) = {
            let a = cur(agent);
            (a.cfg.clone(), a.hooks.clone(), a.store.clone())
        };
        let registry = Arc::new(veles_core::tools::builtin_registry(
            store.clone(),
            None,
            sid,
        ));
        match Agent::new(cfg, hooks, registry, store.clone(), sid.to_string()) {
            Ok(fresh) => {
                if let Some(ask_tx) = self.ask_tx.clone() {
                    fresh.set_asker(Arc::new(veles_core::ask::ChannelAsker { tx: ask_tx }));
                }
                // Repoint the handle. A turn already running keeps the
                // Arc it started with, which is right: a turn belongs to
                // the session that launched it.
                *agent.lock_ok() = Arc::new(fresh);
                self.session = sid.to_string();
                let cfg = cur(agent).cfg.clone();
                reload_from_store(self, store.lock().ok().as_deref(), history, sid, &cfg);
                self.push_msg_status(veles_core::i18n::lookup(
                    "session %{sid}",
                    &[("sid", sid.to_string())],
                ));
                refresh_sessions(self, agent);
            }
            Err(e) => {
                self.push_msg_status(veles_core::i18n::lookup(
                    "switch failed: %{e}",
                    &[("e", format!("{e:#}").to_string())],
                ));
            }
        }
    }
}

/// The knob that mutes an injection class, for the toggle chip a
/// labelled block carries — the same mapping the desktop renders
/// (`desktop_api::injection_toggle`); both must answer alike.
fn injection_toggle_for(
    class: &str,
    cfg: &veles_core::config::VelesConfig,
) -> Option<(String, String)> {
    let (key, on) = match class {
        "background" => (
            "agent.injections.background",
            cfg.agent.injections.background,
        ),
        "todo" => ("agent.injections.todo", cfg.agent.injections.todo),
        "plan" => ("agent.injections.plan", cfg.agent.injections.plan),
        "hook" => ("agent.injections.hook", cfg.agent.injections.hook),
        "irc_context" => (
            "agent.injections.irc_context",
            cfg.agent.injections.irc_context,
        ),
        "verification" => (
            "[agent.guards] verification_nudge",
            cfg.agent.guards.verification_nudge,
        ),
        _ => return None,
    };
    Some((key.to_string(), on.to_string()))
}

/// Rebuild transcript AND conversation history from the store — the
/// reload path. One builder for both (`transcript::from_history`), so
/// a reopened session lays out exactly like the live one did: tool
/// rounds, notes at the point they happened, injected rows labelled.
fn reload_from_store(
    st: &mut UiState,
    store: Option<&veles_core::store::Store>,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    sid: &str,
    cfg: &veles_core::config::VelesConfig,
) {
    let Some(store) = store else {
        st.transcript.clear();
        st.cache.invalidate_all();
        return;
    };
    // RAW rows for the transcript: they are zipped row-for-row against
    // the id and time vectors, and the repaired `load_history` hides
    // orphaned results, which shifted every later row's id and time.
    let (msgs, ids, times, notes) = (
        store.load_history_since(sid, 0).unwrap_or_default(),
        store.message_ids(sid).unwrap_or_default(),
        store.message_times(sid).unwrap_or_default(),
        store.notes_for(sid).unwrap_or_default(),
    );
    // The conversation the NEXT turn continues: user + assistant rows,
    // exactly what the live path accumulated — from the REPAIRED load,
    // since this is what goes on the wire.
    *history.lock_ok() = store
        .load_history(sid)
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .collect();
    st.transcript = veles_core::transcript::from_history(&msgs, &ids, &times, &notes, &|class| {
        injection_toggle_for(class, cfg)
    });
    st.cache.invalidate_all();
}

/// Load the current session's history into the transcript.
fn reload_session(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    sid: &str,
) {
    let a = cur(agent);
    let (store, cfg) = (a.store.clone(), a.cfg.clone());
    drop(a);
    reload_from_store(st, store.lock().ok().as_deref(), history, sid, &cfg);
    refresh_sessions(st, agent);
}

fn refresh_sessions(st: &mut UiState, agent: &AgentHandle) {
    let store = cur(agent).store.clone();
    let metas = store
        .lock()
        .ok()
        .and_then(|s| s.session_meta(50).ok())
        .unwrap_or_default();
    st.sessions = metas
        .into_iter()
        .map(|m| {
            let id = m.id.clone();
            SessionItem {
                id,
                label: if m.title.is_empty() { m.id } else { m.title },
                count: m.message_count,
            }
        })
        .collect();
    if !st.sessions.is_empty() {
        st.sel = st
            .sessions
            .iter()
            .position(|s| s.id == st.session)
            .unwrap_or(0);
    }
}

fn delete_session(agent: &AgentHandle, sid: &str) -> Result<()> {
    agent
        .lock()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .delete_session(sid)?;
    Ok(())
}

fn new_session(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
) {
    let id = format!(
        "{}_{}",
        Local::now().format("%Y%m%d_%H%M%S"),
        &uuid::Uuid::new_v4().to_string()[..6]
    );
    agent
        .lock()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .create_session(&id)
        .ok();
    st.switch_to(agent, history, &id);
}

// ── input editing helpers ───────────────────────────────────────────

fn prev_boundary(s: &str, caret: usize) -> usize {
    s[..caret]
        .char_indices()
        .next_back()
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_boundary(s: &str, caret: usize) -> usize {
    s[caret..]
        .char_indices()
        .nth(1)
        .map(|(i, _)| caret + i)
        .unwrap_or(s.len())
}

fn kill_word(s: &mut String, caret: usize) -> usize {
    let head = &s[..caret];
    let mut end = caret;
    // skip trailing spaces, then a word
    let mut chars = head.char_indices().rev();
    for (i, c) in chars.by_ref() {
        if c.is_whitespace() {
            end = i;
        } else {
            break;
        }
    }
    let mut start = end;
    for (i, c) in chars {
        if c.is_whitespace() {
            break;
        }
        start = i;
    }
    s.replace_range(start..caret, "");
    start
}

// ── key & mouse handling ────────────────────────────────────────────

/// Handle a normal-mode key. Returns true when the app should quit.
/// Dispatch a normal-mode key by the active keymap. Returns true when
/// the app should quit.
fn handle_key(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    key: event::KeyEvent,
) -> bool {
    match st.keymap {
        veles_core::keymap::Keymap::Normie => handle_normie(st, agent, history, ui_tx, key),
        veles_core::keymap::Keymap::Emacs => handle_emacs(st, agent, history, ui_tx, key),
        veles_core::keymap::Keymap::Vi => handle_vi(st, agent, history, ui_tx, key),
    }
}

// ── shared editing + app helpers (used by all three keymaps) ─────────

fn insert_newline(st: &mut UiState) {
    push_undo_before_edit(st);
    st.input.insert(st.caret, '\n');
    st.caret += 1;
    st.last_edit = LastEdit::Other;
}

// ── multiline composer geometry (pure, so the tests can pin it) ──────

/// (line index, byte offset within that line) of a caret position.
/// A caret at a line boundary belongs to the line it ENDS (the last
/// position of line n, not the first of n+1) — that is where typing
/// and backspace feel right after Up/Home.
fn caret_line_col(input: &str, caret: usize) -> (usize, usize) {
    let caret = caret.min(input.len());
    let mut line = 0usize;
    let mut start = 0usize;
    for (i, b) in input.bytes().enumerate() {
        if i >= caret {
            break;
        }
        if b == b'\n' {
            line += 1;
            start = i + 1;
        }
    }
    (line, caret - start)
}

/// Byte index where line `n` starts (line count is clamped, so this is
/// always a valid boundary — the end for out-of-range lines).
fn line_start(input: &str, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut line = 0usize;
    for (i, b) in input.bytes().enumerate() {
        if b == b'\n' {
            line += 1;
            if line == n {
                return i + 1;
            }
        }
    }
    input.len()
}

/// The composer box height for `rows` lines of input: borders + rows,
/// growing with the content but never past half the terminal (the
/// transcript keeps at least its half) and never below the classic
/// single-line 3.
fn composer_height(rows: usize, term_h: u16) -> u16 {
    let max_rows = (term_h / 2).saturating_sub(2).max(1);
    (rows as u16).clamp(1, max_rows) + 2
}

/// Clamp the vertical scroll so the caret's line is on screen: scroll
/// follows the caret down, snaps back up only as far as needed.
fn ensure_caret_row_visible(vscroll: u16, caret_row: usize, visible: u16) -> u16 {
    let visible = visible.max(1) as usize;
    let v = vscroll as usize;
    if caret_row < v {
        caret_row as u16
    } else if caret_row >= v + visible {
        (caret_row + 1 - visible) as u16
    } else {
        vscroll
    }
}

/// Horizontal scroll for the caret's row: 0 while the caret fits, else
/// the prefix width that falls off the left edge (the caret parks one
/// cell from the right edge, editor-style).
fn hscroll_offset(caret_col: usize, width: usize) -> usize {
    if width == 0 || caret_col < width {
        0
    } else {
        caret_col + 1 - width
    }
}

/// The tail of `s` starting after `cols` display columns (char-safe).
fn skip_width(s: &str, cols: usize) -> &str {
    use unicode_width::UnicodeWidthChar;
    let mut seen = 0usize;
    for (i, c) in s.char_indices() {
        if seen >= cols {
            return &s[i..];
        }
        seen += UnicodeWidthChar::width(c).unwrap_or(1);
    }
    ""
}

fn hist_prev(st: &mut UiState) {
    if st.hist.is_empty() {
        return;
    }
    let idx = match st.hist_idx {
        None => st.hist.len() - 1,
        Some(0) => 0,
        Some(i) => i - 1,
    };
    st.hist_idx = Some(idx);
    st.input = st.hist[idx].clone();
    st.caret = st.input.len();
}

fn hist_next(st: &mut UiState) {
    if let Some(i) = st.hist_idx {
        if i + 1 < st.hist.len() {
            st.hist_idx = Some(i + 1);
            st.input = st.hist[i + 1].clone();
            st.caret = st.input.len();
        } else {
            st.hist_idx = None;
            st.input.clear();
            st.caret = 0;
        }
    }
}

fn move_left(st: &mut UiState) {
    st.caret = prev_boundary(&st.input, st.caret);
}
fn move_right(st: &mut UiState) {
    st.caret = next_boundary(&st.input, st.caret);
}
fn home(st: &mut UiState) {
    // Start of the CURRENT line — editor semantics; on a single-line
    // input that is byte 0, exactly the old behavior.
    let (line, _) = caret_line_col(&st.input, st.caret);
    st.caret = line_start(&st.input, line);
}
fn end(st: &mut UiState) {
    let (line, _) = caret_line_col(&st.input, st.caret);
    let s = line_start(&st.input, line);
    st.caret = st.input[s..]
        .find('\n')
        .map(|i| s + i)
        .unwrap_or(st.input.len());
}
/// Byte range of line `n` (start, end-before-newline).
fn line_span(input: &str, n: usize) -> (usize, usize) {
    let s = line_start(input, n);
    let e = input[s..].find('\n').map(|i| s + i).unwrap_or(input.len());
    (s, e)
}
/// Up moves WITHIN a multiline input first; off the TOP line it is
/// history — the readline contract, so the reflexes built on the old
/// single-line composer keep working. Column rides along, clamped by
/// the target line's length.
fn up_from(st: &mut UiState) {
    let (line, col) = caret_line_col(&st.input, st.caret);
    if line == 0 {
        hist_prev(st);
        return;
    }
    let (s, e) = line_span(&st.input, line - 1);
    st.caret = s + col.min(e - s);
}
/// Down: within the input, then history off the BOTTOM line.
fn down_from(st: &mut UiState) {
    let (line, col) = caret_line_col(&st.input, st.caret);
    let last = st.input.split('\n').count() - 1;
    if line >= last {
        hist_next(st);
        return;
    }
    let (s, e) = line_span(&st.input, line + 1);
    st.caret = s + col.min(e - s);
}
fn backspace(st: &mut UiState) {
    if st.caret > 0 {
        push_undo_before_edit(st);
        let b = prev_boundary(&st.input, st.caret);
        st.input.replace_range(b..st.caret, "");
        st.caret = b;
        st.last_edit = LastEdit::Other;
    }
}
fn del_char(st: &mut UiState) {
    if st.caret < st.input.len() {
        push_undo_before_edit(st);
        let b = next_boundary(&st.input, st.caret);
        st.input.replace_range(st.caret..b, "");
        st.last_edit = LastEdit::Other;
    }
}
fn clear_input(st: &mut UiState) {
    push_undo_before_edit(st);
    st.input.clear();
    st.caret = 0;
    st.last_edit = LastEdit::Other;
}
fn kill_word_bwd(st: &mut UiState) {
    push_undo_before_edit(st);
    st.caret = kill_word(&mut st.input, st.caret);
    st.last_edit = LastEdit::Other;
}
fn kill_to_eol(st: &mut UiState) {
    // C-k: kill from caret to end of line into the kill ring.
    if st.caret < st.input.len() {
        push_undo_before_edit(st);
        let rest = st.input[st.caret..].to_string();
        st.yank = rest;
        st.input.truncate(st.caret);
        st.last_edit = LastEdit::Other;
    }
}
fn yank(st: &mut UiState) {
    if !st.yank.is_empty() {
        push_undo_before_edit(st);
        st.input.insert_str(st.caret, &st.yank);
        st.caret += st.yank.len();
        st.last_edit = LastEdit::Other;
    }
}

// ── composer editing + undo (Ctrl-/) ─────────────────────────────────

const UNDO_DEPTH: usize = 100;

/// Snapshot the composer so the next Ctrl-/ can restore it. Called
/// before every edit EXCEPT a continued run of plain character inserts,
/// which coalesce so undoing "the last edit" rolls back the word, not
/// the letter.
fn push_undo_before_edit(st: &mut UiState) {
    if st.undo.len() >= UNDO_DEPTH {
        st.undo.remove(0);
    }
    st.undo.push((st.input.clone(), st.caret));
}

/// The one place printable text enters the composer. Adjacent inserts
/// ("hello" typed at mach speed) coalesce into a single undo step; any
/// other edit in between starts a new one.
fn composer_insert(st: &mut UiState, text: &str) {
    if st.last_edit != LastEdit::CharRun {
        push_undo_before_edit(st);
    }
    st.input.insert_str(st.caret, text);
    st.caret += text.len();
    st.last_edit = LastEdit::CharRun;
}

/// Ctrl-/ (and Ctrl-_, the same 0x1F byte on hostile terminals): undo
/// the last edit. Restoring a snapshot ends the current char run, so
/// the next insert snapshots afresh rather than rebuilding on top of a
/// reverted string.
fn composer_undo(st: &mut UiState) {
    if let Some((input, caret)) = st.undo.pop() {
        st.input = input;
        st.caret = caret;
        st.last_edit = LastEdit::Other;
    }
}
fn word_left(st: &mut UiState) {
    // M-b: back to the start of the current/previous word.
    let head = &st.input[..st.caret];
    let mut i = st.caret;
    let mut chars = head.char_indices().rev().peekable();
    // skip non-word chars
    while let Some((idx, c)) = chars.peek() {
        if c.is_alphanumeric() || *c == '_' {
            break;
        }
        i = *idx;
        chars.next();
    }
    for (idx, c) in chars {
        if !(c.is_alphanumeric() || c == '_') {
            break;
        }
        i = idx;
    }
    st.caret = i;
}
fn word_right(st: &mut UiState) {
    let tail = &st.input[st.caret..];
    let mut i = st.caret;
    let mut chars = tail.char_indices();
    for (off, c) in chars.by_ref() {
        if c.is_alphanumeric() || c == '_' {
            i = st.caret + off + c.len_utf8();
            break;
        }
    }
    for (off, c) in chars {
        if !(c.is_alphanumeric() || c == '_') {
            break;
        }
        i = st.caret + off + c.len_utf8();
    }
    st.caret = i;
}

fn interrupt_or_quit(st: &mut UiState) -> bool {
    // Ctrl-C / Esc: cancel a running turn, else quit. Returns quit.
    if st.busy {
        if let Some(c) = &st.cancel {
            c.cancel();
            st.push_msg_status(veles_core::i18n::lookup("interrupting…", &[]));
        }
        false
    } else {
        true
    }
}

/// Ctrl-Z: real job-control suspend. Raw mode swallows the terminal's
/// own SIGTSTP (ISIG is off), so the app must raise it itself — after
/// leaving the raw mode and the alt-screen, or the shell you drop into
/// is garbage and the TUI paints over it on resume. The raise blocks
/// until the shell fg's us back (SIGCONT), then the terminal is
/// re-entered; the event loop's next draw repaints the alt-screen.
fn suspend_terminal() -> std::io::Result<()> {
    let mut out = io::stdout();
    execute!(
        out,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    disable_raw_mode()?;
    // SAFETY: raise(SIGTSTP) is the job-control idiom; the process
    // stops until the foreground shell continues it. Windows has no
    // job control: Ctrl-Z leaves the screen as it is and returns.
    #[cfg(unix)]
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
    enable_raw_mode()?;
    execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    execute!(out, crossterm::cursor::Hide)?;
    Ok(())
}

/// The REAL /compact, off the UI thread like a turn: one summarizer
/// call, archive the head, insert the marker — the same engine path the
/// automatic trigger, the desktop's /compress and the REPL's use. The
/// old TUI arm hardcoded `archive_head(sid, 10)` — no summarizer, no
/// marker — and never updated the in-memory history either, so store
/// and memory desynced and the NEXT compact archived the wrong rows.
/// The result surfaces as an engine NOTE, not a reply: housekeeping
/// must not read as something the model said.
fn spawn_compact(
    agent: AgentHandle,
    history: Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    tx: mpsc::Sender<UiEvent>,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(UiEvent::Note(Box::new(veles_core::notes::EngineNote::new(
                    veles_core::notes::NoteKind::Compaction,
                    format!("compaction failed: runtime: {e}"),
                ))));
                return;
            }
        };
        let note = rt.block_on(async {
            let mut h = history.lock_ok().clone();
            let before = h.len();
            let out = {
                let a = cur(&agent);
                a.compact_forced(&mut h).await
            };
            *history.lock_ok() = h;
            match out {
                Some(outcome) => {
                    let after = history.lock_ok().len();
                    // The shared formatter — the TUI used to print its
                    // own wording ("compacted N → M messages"), which is
                    // how four surfaces ended up describing the same
                    // event four different ways.
                    veles_core::notes::EngineNote::new(
                        veles_core::notes::NoteKind::Compaction,
                        outcome.report(before, after),
                    )
                    .with_detail(
                        "Run by /compress. Older messages were summarized and archived, \
                         not deleted — session_search still finds them, pruned tool \
                         outputs included.",
                    )
                }
                None => veles_core::notes::EngineNote::new(
                    veles_core::notes::NoteKind::Compaction,
                    "nothing to compact — history too short for a legal cut".to_string(),
                ),
            }
        });
        let _ = tx.send(UiEvent::Note(Box::new(note)));
    });
}

fn send_current(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
) {
    // /quit works even mid-turn: cancel whatever is running, then go.
    // (local_command cannot express app-level actions — it answers
    // from engine state — so the ones that act on the app live here.)
    if matches!(st.input.trim(), "/quit" | "/exit") {
        if st.busy {
            if let Some(c) = st.cancel.take() {
                c.cancel();
            }
        }
        st.input.clear();
        st.caret = 0;
        st.quit = true;
        return;
    }
    // /compact: an LLM call like a turn — spawned off the UI thread,
    // result surfaces as an engine note (see spawn_compact).
    if matches!(st.input.trim(), "/compress" | "/compact") {
        st.input.clear();
        st.caret = 0;
        spawn_compact(agent.clone(), history.clone(), ui_tx.clone());
        st.push_msg_status(veles_core::i18n::lookup("compacting…", &[]));
        return;
    }
    // Mid-turn steering: the TUI keeps accepting input while a turn
    // runs, so /steer can land mid-turn the way the help promises
    // (the REPL physically cannot read input mid-turn; desktop has its
    // own endpoint).
    if st.busy {
        let steer = st
            .input
            .trim()
            .strip_prefix("/steer")
            .map(|m| m.trim().to_string());
        if let Some(msg) = steer {
            st.input.clear();
            st.caret = 0;
            if msg.is_empty() {
                st.push_msg_status(veles_core::i18n::lookup("usage: /steer <instruction>", &[]));
            } else if let Some(agent) = st.agent_snapshot.as_ref() {
                cur(agent).pending_steer.lock_ok().replace(msg.clone());
                st.push_msg_status(veles_core::i18n::lookup(
                    "steer queued: '%{msg}' — will inject after the next tool call",
                    &[("msg", msg.to_string())],
                ));
            }
            return;
        }
        // The rest of mid-turn input (R4.6): shared commands still
        // answer from engine state — /queue itself included — and
        // plain text is NOT a silent no-op any more. It becomes a
        // queue row every surface sees, drained at the turn boundary.
        let text = st.input.trim().to_string();
        if text.starts_with('/') {
            let (head, rest) = match text.split_once(' ') {
                Some((h, r)) => (h, r.trim()),
                None => (text.as_str(), ""),
            };
            {
                let a = cur(agent);
                let mut h = history.lock_ok();
                let mut ctx = super::commands::CommandCtx {
                    agent: &a,
                    surface: super::commands::Surface::Tui,
                    history: Some(&mut *h),
                    turn_live: true,
                    principal: None,
                };
                if let Some(out) = super::commands::dispatch(&mut ctx, head, rest) {
                    drop(a);
                    st.input.clear();
                    st.caret = 0;
                    st.push_msg_status(out.text);
                    return;
                }
            }
            st.push_msg_status(veles_core::i18n::lookup(
                "a turn is running — /queue parks text, /steer redirects it",
                &[],
            ));
            return;
        }
        if text.is_empty() {
            return;
        }
        st.input.clear();
        st.caret = 0;
        let a = cur(agent);
        let sid = a.session_id.clone();
        match a.store.lock_ok().queue_push(&sid, &text) {
            Ok(seq) => st.push_msg_status(veles_core::i18n::lookup(
                "queued #%{seq} — runs when this turn ends (/queue to see or cancel)",
                &[("seq", seq.to_string())],
            )),
            Err(e) => st.push_msg_status(veles_core::i18n::lookup(
                "queue failed: %{e}",
                &[("e", e.to_string())],
            )),
        }
        return;
    }
    if st.input.trim().is_empty() {
        return;
    }
    let text = st.input.trim().to_string();
    st.input.clear();
    st.caret = 0;
    st.hist_idx = None;
    // `/resume <id-or-prefix>`: app-level like /new — it rebuilds the
    // agent handle, which local_command (engine-state answers) cannot
    // reach. Prefix-matched against the store, same as the REPL.
    if text == "/resume" || text.starts_with("/resume ") {
        let rest = text.strip_prefix("/resume").unwrap().trim();
        if rest.is_empty() {
            st.push_msg_status(veles_core::i18n::lookup(
                "usage: /resume <id-or-prefix>",
                &[],
            ));
            return;
        }
        let a = cur(agent);
        let found = {
            let store = a.store.lock_ok();
            store.find_session(rest).unwrap_or_default()
        };
        drop(a);
        match found {
            Some(id) => {
                st.switch_to(agent, history, &id);
            }
            None => st.push_msg_status(veles_core::i18n::lookup(
                "no session matching '%{rest}'",
                &[("rest", rest.to_string())],
            )),
        }
        return;
    }
    // A CHAT alias — `/qwen-flash <prompt>`. Beside `/new` for the same
    // reason: it spins a session and rebinds the handle, which the shared
    // dispatcher's `&Agent` cannot do. Media aliases were already
    // answered by `commands::dispatch` above.
    //
    // `resolve_command` and not `resolve`: this point sees ALL input, not
    // only commands, and the lenient lookup would let an alias named
    // `foo` hijack any message whose first word is `foo`.
    let (alias_head, alias_rest) = match text.split_once(' ') {
        Some((h, r)) => (h, r.trim()),
        None => (text.as_str(), ""),
    };
    if let Some(al) = veles_core::aliases::resolve_command(&cur(agent).cfg, alias_head) {
        if al.kind == veles_core::aliases::AliasKind::Chat {
            if st.busy {
                st.push_msg_status(veles_core::i18n::lookup(
                    "a turn is running — /cancel first",
                    &[],
                ));
                return;
            }
            new_session(st, agent, history);
            // On the FRESH agent: `new_session` rebinds, and a fork
            // carries the previous session's overrides forward, so
            // setting the route on the old one leaves the new session
            // where it was.
            match cur(agent).set_model(&al.model_arg()) {
                Ok(route) => st.push_msg_status(veles_core::i18n::lookup(
                    "(%{route})",
                    &[("route", route.to_string())],
                )),
                Err(e) => {
                    st.push_msg_status(veles_core::i18n::lookup(
                        "(route did not resolve: %{e})",
                        &[("e", format!("{e:#}").to_string())],
                    ));
                    return;
                }
            }
            if let Some(r) = &al.reasoning {
                cur(agent).set_reasoning(r);
            }
            if !alias_rest.is_empty() {
                launch_turn(st, agent, history, ui_tx, alias_rest.to_string());
            }
            return;
        }
    }
    // App-level commands that need more than the store: /new spins a
    // session, /sessions lists the sidebar. Unknown slash text used to
    // fall through to the MODEL here — these four names were in /help
    // and typing them shipped "/quit" as a prompt.
    match text.as_str() {
        "/new" => {
            new_session_cmd(st, agent, history);
            return;
        }
        "/retry" => {
            // Resend the last prompt as a fresh turn — the REPL's
            // run_prompt_once, in the TUI's spawn shape.
            let Some(last) = cur(agent).last_prompt() else {
                st.push_msg_status(veles_core::i18n::lookup("nothing to retry", &[]));
                return;
            };
            st.push_msg_status(veles_core::i18n::lookup("(retrying last prompt…)", &[]));
            launch_turn(st, agent, history, ui_tx, last);
            return;
        }
        "/copy" => {
            copy_last_reply(st, false);
            return;
        }
        "/paste" => match read_clipboard() {
            Some(t) if !t.is_empty() => {
                st.input.insert_str(st.caret, &t);
                st.caret += t.len();
                st.push_msg_status(format!("(pasted {} chars into the input)", t.len()));
            }
            _ => st.push_msg_status(veles_core::i18n::lookup(
                "(clipboard empty or no clipboard tool)",
                &[],
            )),
        },
        _ => {}
    }
    // S6: custom commands render to a prompt that REPLACES the raw
    // slash text (owner surface — expansion is trusted). Since
    // 2026-09-03 commands come from .md files OR plugin registration
    // (`veles::register_command`); Tcl-backed ones hop through the
    // plugins interpreter (sync — the TUI thread is not in a runtime).
    let text = if text.starts_with('/') {
        let render_one = |name: &str, args: &str| -> Option<String> {
            match veles_core::commands::find_any(name) {
                Some(veles_core::commands::ResolvedCommand::Disk(c)) => {
                    Some(veles_core::commands::render(&c, args))
                }
                Some(veles_core::commands::ResolvedCommand::Plugin(t)) => {
                    match veles_core::commands::render_plugin_blocking(&t, args) {
                        Ok(s) => Some(s),
                        Err(e) => Some(format!("(command /{} failed: {e})", t.name)),
                    }
                }
                None => None,
            }
        };
        match text.split_once(' ') {
            Some((h, r)) => render_one(h, r.trim()).unwrap_or(text),
            None => render_one(&text, "").unwrap_or(text),
        }
    } else {
        text
    };
    // Local commands: answered from the store, no turn spent.
    if let Some(out) = local_command(st, agent, history, &text) {
        st.hist.push(text.clone());
        st.emit(TEvent::User {
            text: text.clone(),
            time: now_hms(),
            injection: None,
        });
        st.push_msg_status(out);
        return;
    }
    launch_turn(st, agent, history, ui_tx, text);
}

/// Run a row the queue handed over. Mail (`veles_core::mail`) is
/// painted as mail — its text under its sender's label — and kept out of
/// the composer's history, because the human did not type it; the turn
/// is handed the whole body, which `run_turn` claims. Anything else is a
/// queued message and launches like one.
fn launch_queued(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    item: veles_core::store::QueueItem,
) {
    let mail = (item.kind.as_deref() == Some("mail"))
        .then(|| veles_core::mail::parse_body(&item.body))
        .flatten()
        .map(|(id, text)| (id.to_string(), text.to_string()));
    let Some((id, text)) = mail else {
        return launch_turn(st, agent, history, ui_tx, item.body);
    };
    let from = cur(agent)
        .store
        .lock_ok()
        .mail_get(&id)
        .ok()
        .flatten()
        .map(|r| veles_core::mail::injection_tag(&r))
        .unwrap_or_else(|| "mail:?".to_string());
    st.emit(TEvent::User {
        text,
        time: now_hms(),
        injection: Some(veles_core::transcript::Injection {
            class: from,
            toggle: None,
        }),
    });
    start_turn(st, agent, history, ui_tx, item.body);
}

/// Echo the prompt and run it as a spawned turn — the one launch path
/// for the composer AND for `/retry`, which is the same thing with
/// yesterday's text.
fn launch_turn(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    text: String,
) {
    st.hist.push(text.clone());
    st.emit(TEvent::User {
        text: text.clone(),
        time: now_hms(),
        injection: None,
    });
    start_turn(st, agent, history, ui_tx, text);
}

/// The turn itself, once the prompt is painted.
fn start_turn(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    text: String,
) {
    // No pre-created empty reply and no stream index: the first Delta
    // opens the block itself, and a tool round closing it is exactly
    // what makes round two land BELOW the tool.
    st.busy = true;
    st.route.clear();
    st.pending_ask = None;
    st.scrollback = 0;
    let cancel = veles_core::providers::CancelToken::new();
    st.cancel = Some(cancel.clone());
    spawn_turn(agent.clone(), history.clone(), text, ui_tx.clone(), cancel);
}

fn new_session_cmd(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
) {
    if !st.busy {
        new_session(st, agent, history);
    }
}

fn cycle_session(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    dir: i32,
) {
    if st.sessions.is_empty() || st.busy {
        return;
    }
    let n = st.sessions.len() as i32;
    st.sel = ((st.sel as i32 + dir + n) % n) as usize;
    let next = st.sessions[st.sel].id.clone();
    st.switch_to(agent, history, &next);
}

fn delete_session_cmd(st: &mut UiState) {
    if st.input.is_empty() && !st.sessions.is_empty() && !st.busy {
        let sid = st.sessions[st.sel].id.clone();
        st.confirm_delete = Some(sid);
    } else if st.caret < st.input.len() {
        let b = next_boundary(&st.input, st.caret);
        st.input.replace_range(st.caret..b, "");
    }
}

/// Read the system clipboard for `/paste`. OSC52 (used by `/copy`)
/// writes through the terminal, but reading needs a helper binary:
/// try the X11 tools, then Wayland, mirroring the REPL's /paste.
fn read_clipboard() -> Option<String> {
    veles_core::clipboard::read()
}

fn copy_last_reply(st: &mut UiState, want_code: bool) {
    let last = st
        .transcript
        .blocks
        .iter()
        .rev()
        .find(|b| b.kind == Kind::Reply)
        .map(|b| b.body.clone());
    match last {
        None => st.push_msg_status(veles_core::i18n::lookup("nothing to copy yet", &[])),
        Some(text) => {
            let payload = if want_code {
                match last_code_block(&text) {
                    Some(c) => c,
                    None => {
                        st.push_msg_status(veles_core::i18n::lookup(
                            "no code block in the last reply",
                            &[],
                        ));
                        return;
                    }
                }
            } else {
                text
            };
            match osc52_copy(&payload) {
                Ok(()) => st.push_msg_status(format!(
                    "copied {} bytes{}",
                    payload.len(),
                    if want_code { " (code block)" } else { "" }
                )),
                Err(e) => st.push_msg_status(veles_core::i18n::lookup(
                    "copy failed: %{e}",
                    &[("e", e.to_string())],
                )),
            }
        }
    }
}

// ── M-x command palette (all keymaps) ────────────────────────────────

#[derive(Default)]
struct PaletteState {
    query: String,
    sel: usize,
}

/// A modal list editor — the "editors, not dumps" shape. Each row shows
/// a `label`, feeds its `value` to an action, and may carry a `detail`
/// for the pager. Enter runs the row's own action or the picker's when
/// it has none. /model is the first consumer; Review, Files and the
/// collection panes reuse the same shape.
struct Picker {
    title: String,
    rows: Vec<PickerRow>,
    sel: usize,
    /// The action a row runs when it carries none of its own.
    kind: PickerAction,
}

#[derive(Clone)]
struct PickerRow {
    label: String,
    value: String,
    /// Full text for the `View` pager (a diff, a file, a SKILL body…).
    detail: Option<String>,
    /// Per-row override; `None` falls back to `Picker::kind`.
    action: Option<PickerAction>,
}

#[derive(Clone)]
enum PickerAction {
    /// Feed the row's value through `agent.set_model`.
    SetModel,
    /// Run the row's value as a slash command (the `local_command` path).
    Command,
    /// Open the row's detail in the scrollable DetailView.
    View,
}

/// A scrollable read-only pager — the "detail" half of an editor.
/// Enter on a `View` row opens one over whatever it pointed at (a diff,
/// a file head, a SKILL body); Esc/q closes it.
struct DetailView {
    title: String,
    text: String,
    /// First visible line (0 = top), clamped each draw.
    scroll: usize,
}

/// The model picker: current model first, then every configured alias,
/// Enter runs it through the same `set_model` the `/model <name>` shared
/// command uses (alias, custom-provider and reasoning resolution all
/// stay in one place).
fn model_picker(agent: &AgentHandle) -> Picker {
    let a = cur(agent);
    let current = a
        .overrides
        .read_ok()
        .model
        .clone()
        .or_else(|| a.cfg.model.default.clone())
        .unwrap_or_default();
    // Aliases are the things a user can actually pick by name; the
    // default + current model are surfaced even if no alias covers them.
    let mut names: Vec<String> = a.cfg.model_aliases.keys().cloned().collect();
    names.sort();
    let mut rows: Vec<PickerRow> = Vec::new();
    let mut sel = 0usize;
    if !current.is_empty() {
        rows.push(PickerRow {
            label: format!("{current} (current)"),
            value: current.clone(),
            detail: None,
            action: None,
        });
    }
    for name in names {
        if name == current {
            continue;
        }
        rows.push(PickerRow {
            label: name.clone(),
            value: name,
            detail: None,
            action: None,
        });
    }
    if rows.is_empty() {
        rows.push(PickerRow {
            label: "(no models configured — /model <name> to set)".to_string(),
            value: String::new(),
            detail: None,
            action: None,
        });
        sel = 0;
    } else if let Some(cur_sel) = rows
        .iter()
        .position(|r| r.value == current && !current.is_empty())
    {
        sel = cur_sel;
    }
    Picker {
        title: "model".to_string(),
        rows,
        sel,
        kind: PickerAction::SetModel,
    }
}

/// Cap a blob before it becomes a pager row: past this a terminal is not
/// a reader, and a 12MB memory store would otherwise be cloned per key.
fn cap(text: &str, bytes: usize) -> String {
    if text.len() <= bytes {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(bytes - 3).collect();
    kept.push('…');
    kept
}

/// First N lines of a file, or an honest "cannot/binary/too large" note.
/// The Files browser and the `open_preview` TUI target share this so a
/// file preview reads identically wherever it is asked for.
fn file_head(path: &str, lines: usize) -> String {
    let p = std::path::Path::new(path);
    let meta = match std::fs::metadata(p) {
        Ok(m) => m,
        Err(e) => return format!("(cannot read {path}: {e})"),
    };
    if meta.len() > 2_000_000 {
        return format!("({path}: file too large to preview — {} bytes)", meta.len());
    }
    match std::fs::read(p) {
        Ok(bytes) => {
            if bytes.iter().take(8000).any(|&b| b == 0) {
                return format!("({path}: binary, {} bytes)", bytes.len());
            }
            let text = String::from_utf8_lossy(&bytes).to_string();
            let shown = text.lines().take(lines).collect::<Vec<_>>().join("\n");
            let capped = cap(&shown, 200_000);
            if capped.len() < text.len() {
                format!("{capped}\n…")
            } else {
                capped
            }
        }
        Err(e) => format!("(cannot read {path}: {e})"),
    }
}

/// The /checkpoints picker: one row per checkpoint, Enter restores it
/// through the shared `/restore <id>` (so restore's own error handling
/// and report both apply). A checkpoint IS an undo point — restoring one
/// is the thing it exists for, not a destructive edit.
fn checkpoints_picker(agent: &AgentHandle) -> Picker {
    let a = cur(agent);
    let items = a
        .store
        .lock_ok()
        .checkpoint_list(Some(&a.session_id), 20)
        .unwrap_or_default();
    let rows: Vec<PickerRow> = items
        .iter()
        .map(|c| PickerRow {
            label: format!(
                "#{}  {}  {}  {}",
                c.id,
                c.tool,
                truncate(&c.path, 40),
                c.created_at
            ),
            value: format!("/restore {}", c.id),
            detail: Some(format!(
                "checkpoint #{}\npath: {}\ntool: {}\nbytes: {}\ncreated: {}\nnote: {}",
                c.id,
                c.path,
                c.tool,
                c.bytes,
                c.created_at,
                c.note.as_deref().unwrap_or("—"),
            )),
            action: Some(PickerAction::Command),
        })
        .collect();
    if rows.is_empty() {
        return Picker {
            title: "checkpoints".into(),
            rows: vec![PickerRow {
                label: "(no checkpoints in this session)".into(),
                value: String::new(),
                detail: None,
                action: None,
            }],
            sel: 0,
            kind: PickerAction::View,
        };
    }
    Picker {
        title: format!("checkpoints ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::Command,
    }
}

/// The /skills picker: one row per skill, Enter opens its full SKILL.md
/// in the pager (the same body `skill_view` serves the model).
fn skills_picker() -> Picker {
    let mut set = veles_core::skills::SkillSet::load();
    set.skills
        .retain(|s| !s.category.is_empty() || !s.name.is_empty());
    let rows: Vec<PickerRow> = set
        .skills
        .iter()
        .map(|s| PickerRow {
            label: format!("{:<20} {}", s.name, truncate(&s.description, 44)),
            value: s.name.clone(),
            detail: Some(veles_core::skills::view_skill(
                &set, &s.name, None, None, None,
            )),
            action: Some(PickerAction::View),
        })
        .collect();
    if rows.is_empty() {
        return Picker {
            title: "skills".into(),
            rows: vec![PickerRow {
                label: format!(
                    "(no skills — drop a SKILL.md in {}/<name>/)",
                    veles_core::config::config_dir().join("skills").display()
                ),
                value: String::new(),
                detail: None,
                action: None,
            }],
            sel: 0,
            kind: PickerAction::View,
        };
    }
    Picker {
        title: format!("skills ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The /plugins picker: one row per plugin, Enter shows its manifest +
/// load path in the pager.
fn plugins_picker(agent: &AgentHandle) -> Picker {
    let a = cur(agent);
    let found = veles_core::plugins::discover(&a.cfg.plugins.enabled, &a.cfg.plugins.disabled)
        .unwrap_or_default();
    let rows: Vec<PickerRow> = found
        .iter()
        .map(|p| PickerRow {
            label: format!("{:<20} kind={}", p.manifest.name, p.manifest.kind),
            value: p.manifest.name.clone(),
            detail: Some(format!(
                "plugin: {}\nkind: {}\nentry: {}\ndir: {}\nprovides_hooks: {}\ndescription: {}",
                p.manifest.name,
                p.manifest.kind,
                p.manifest.entry,
                p.dir.display(),
                p.manifest.provides_hooks.join(", "),
                if p.manifest.description.is_empty() {
                    "—"
                } else {
                    &p.manifest.description
                },
            )),
            action: Some(PickerAction::View),
        })
        .collect();
    if rows.is_empty() {
        return Picker {
            title: "plugins".into(),
            rows: vec![PickerRow {
                label: "(no plugins)".into(),
                value: String::new(),
                detail: None,
                action: None,
            }],
            sel: 0,
            kind: PickerAction::View,
        };
    }
    Picker {
        title: format!("plugins ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The /hooks picker: the hook taxonomy, one row per hook, Enter shows
/// which surfaces fire it and whether anything listens.
fn hooks_picker() -> Picker {
    let rows: Vec<PickerRow> = veles_core::hooks::KNOWN_HOOKS
        .iter()
        .map(|h| {
            let fired = veles_core::hooks::FIRED_HOOKS.contains(h);
            let listens = veles_core::hooks::has_handler(h);
            PickerRow {
                label: format!(
                    "{h:<26} {} {}",
                    if fired { "fires" } else { "reserved" },
                    if listens { "handler" } else { "—" }
                ),
                value: (*h).to_string(),
                detail: Some(format!(
                    "hook: {h}\nfired by the engine: {}\nlistener attached: {}\n",
                    if fired {
                        "yes"
                    } else {
                        "no (reserved for hermes plugins)"
                    },
                    if listens { "yes (rust or tcl)" } else { "no" },
                )),
                action: Some(PickerAction::View),
            }
        })
        .collect();
    Picker {
        title: format!("hooks ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The /memory picker: the two resident stores, then every other memory
/// file, each with its size and last change; Enter reads one in the
/// pager. Editing is `veles memory edit <path>` — the title says so,
/// because since memory.d is gone there is no file to open instead.
fn memory_picker() -> Picker {
    let mem = veles_core::memory::MemoryStore::new();
    let m = mem.read(veles_core::memory::StoreKind::Memory);
    let u = mem.read(veles_core::memory::StoreKind::User);
    let mut rows = vec![
        PickerRow {
            label: format!("MEMORY  ({m_len} chars)  system/core.md", m_len = m.len()),
            value: "memory".into(),
            detail: Some(cap(&m, 80_000)),
            action: Some(PickerAction::View),
        },
        PickerRow {
            label: format!("USER    ({u_len} chars)  system/user.md", u_len = u.len()),
            value: "user".into(),
            detail: Some(cap(&u, 80_000)),
            action: Some(PickerAction::View),
        },
    ];
    for f in mem.list().unwrap_or_default() {
        if matches!(f.path.as_str(), "system/core.md" | "system/user.md") {
            continue;
        }
        let body = mem.read_file(&f.path).unwrap_or_default();
        rows.push(PickerRow {
            label: format!("{}  ({} chars, {})", f.path, f.chars, f.at),
            value: f.path.clone(),
            detail: Some(cap(&body, 80_000)),
            action: Some(PickerAction::View),
        });
    }
    Picker {
        title: format!(
            "memory — {} file(s); edit one with `veles memory edit <path>`",
            rows.len()
        ),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The /acl picker: owner surface summary, every rule the FILE declares,
/// every rule the STORE holds, and the two promote lines.
///
/// Enter on a rule explains it; Enter on a promote line runs it through
/// the same `local_command` path `/restore` uses, so the transcript shows
/// the engine's own report rather than a pane's paraphrase of it. The dry
/// run is a row of its own and comes first: a picker that can write
/// config.toml should be able to show what it would write.
fn acl_picker(agent: &AgentHandle) -> Picker {
    let a = cur(agent);
    let cfg = &a.cfg;
    let grants = veles_core::pairing::list_grants(&a.store);
    let users = veles_core::pairing::list_users(&a.store);
    let mut rows: Vec<PickerRow> = Vec::new();
    rows.push(PickerRow {
        label: "owner surface — full caps".into(),
        value: String::new(),
        detail: Some(format!(
            "owner surface: full capabilities\nconfigured: {}\ndefault role: {}\nruntime: {} \
             grant(s), {} user(s)",
            cfg.acl.is_configured(),
            cfg.acl.default_role.as_deref().unwrap_or("(none)"),
            grants.len(),
            users.len(),
        )),
        action: Some(PickerAction::View),
    });
    for owner in &cfg.acl.owner {
        rows.push(PickerRow {
            label: format!("owner  {owner}"),
            value: owner.clone(),
            detail: Some("role: owner\norigin: config [acl] owners".to_string()),
            action: Some(PickerAction::View),
        });
    }
    for (id, role) in &cfg.acl.roles {
        rows.push(PickerRow {
            label: format!("pin    {id}"),
            value: id.clone(),
            detail: Some(format!("role: {role}\norigin: config [acl] roles pin")),
            action: Some(PickerAction::View),
        });
    }
    for e in &cfg.acl.entries {
        rows.push(PickerRow {
            label: format!("entry  {} ({})", e.identity, e.role),
            value: e.identity.clone(),
            detail: Some(format!(
                "identity: {}\nrole: {}\nnetwork: {}\nchannel: {}\nplatform: {}\norigin: config \
                 [[acl.entries]]",
                e.identity,
                e.role,
                e.network.as_deref().unwrap_or("any"),
                e.channel.as_deref().unwrap_or("any"),
                e.platform.as_deref().unwrap_or("any"),
            )),
            action: Some(PickerAction::View),
        });
    }
    for u in &cfg.acl.users {
        rows.push(PickerRow {
            label: format!("user   {} ({})", u.name, u.role),
            value: u.name.clone(),
            detail: Some(format!(
                "name: {}\nrole: {}\nidentities: {}\norigin: config [[acl.users]]",
                u.name,
                u.role,
                u.identities.join(" ")
            )),
            action: Some(PickerAction::View),
        });
    }
    for g in grants {
        rows.push(PickerRow {
            label: format!("grant  {} ({})", g.identity, g.role),
            value: g.identity.clone(),
            detail: Some(format!(
                "identity: {}\nrole: {}\nnetwork: {}\nchannel: {}\ngranted (unix): {}\nexpires \
                 (unix): {}\norigin: the runtime store — `/acl promote` writes it into \
                 config.toml",
                g.identity,
                g.role,
                g.network.as_deref().unwrap_or("any"),
                g.channel.as_deref().unwrap_or("any"),
                g.granted_at,
                if g.expires_at == 0 {
                    "never".to_string()
                } else {
                    g.expires_at.to_string()
                }
            )),
            action: Some(PickerAction::View),
        });
    }
    for u in users {
        rows.push(PickerRow {
            label: format!("ruser  {} ({})", u.name, u.role),
            value: u.name.clone(),
            detail: Some(format!(
                "name: {}\nrole: {}\nidentities: {}\norigin: the runtime store — `/acl promote \
                 --users` writes it into config.toml",
                u.name,
                u.role,
                u.identities.join(" ")
            )),
            action: Some(PickerAction::View),
        });
    }
    // The control. A picker that lists the store's rules with no way to
    // keep them can only watch a store reset take them.
    rows.push(PickerRow {
        label: "promote — preview what would be written".into(),
        value: "/acl promote --dry-run --users".into(),
        detail: Some(
            "runs `/acl promote --dry-run --users`: the lines it would append to config.toml, \
             the rules the file already decides, and the ones it refuses — and writes nothing"
                .to_string(),
        ),
        action: Some(PickerAction::Command),
    });
    rows.push(PickerRow {
        label: "promote — write it into config.toml".into(),
        value: "/acl promote --users".into(),
        detail: Some(
            "runs `/acl promote --users`: appends the rules the store decides to config.toml \
             (comments and key order kept, a backup beside the file) and drops the store \
             copies. Refuses while `[acl]` is unconfigured, because the first entry is what \
             makes the ACL curated"
                .to_string(),
        ),
        action: Some(PickerAction::Command),
    });
    Picker {
        title: format!("acl ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The Review (git) picker: `git status --porcelain` is the file list,
/// Enter shows that file's diff (`git diff HEAD -- <path>`) in the pager.
/// Bounded because a repo with a thousand edits is not something a
/// terminal pager is expected to page through one row at a time.
fn diff_picker() -> Picker {
    let Some(status) = git_text(&["status", "--porcelain"]) else {
        return Picker {
            title: "review".into(),
            rows: vec![PickerRow {
                label: "(not a git repo, or git unavailable)".into(),
                value: String::new(),
                detail: None,
                action: None,
            }],
            sel: 0,
            kind: PickerAction::View,
        };
    };
    let mut rows: Vec<PickerRow> = Vec::new();
    for line in status.lines().take(400) {
        let (xy, path) = if line.len() >= 4 {
            line.split_at(2)
        } else {
            ("  ", line)
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        let mut detail = git_text(&["diff", "HEAD", "--", path]).unwrap_or_default();
        if detail.trim().is_empty() {
            detail = "(no unstaged diff — file may be newly added; `git diff --cached -- <path>` would show it)"
                .into();
        }
        rows.push(PickerRow {
            label: format!("{xy}  {}", truncate(path, 64)),
            value: path.to_string(),
            detail: Some(cap(&detail, 200_000)),
            action: Some(PickerAction::View),
        });
    }
    if rows.is_empty() {
        return Picker {
            title: "review".into(),
            rows: vec![PickerRow {
                label: "(no changes — /diff staged or /diff all for other views)".into(),
                value: String::new(),
                detail: None,
                action: None,
            }],
            sel: 0,
            kind: PickerAction::View,
        };
    }
    Picker {
        title: format!("review ({})", rows.len()),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// The Files browser: `..` + directories descend (re-open the picker at
/// the new path via `/files <path>`), files open in the pager. One level
/// at a time, dirs first — the lazy shape the desktop tree also uses.
fn files_picker(path: &str) -> Picker {
    let root = std::path::PathBuf::from(path);
    let mut rows: Vec<PickerRow> = Vec::new();
    if let Some(parent) = root.parent().filter(|p| !p.as_os_str().is_empty()) {
        rows.push(PickerRow {
            label: ".. (parent)".into(),
            value: format!("/files {}", parent.display()),
            detail: None,
            action: Some(PickerAction::Command),
        });
    }
    let mut entries: Vec<_> = match std::fs::read_dir(&root) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            return Picker {
                title: format!("files — {path}"),
                rows: vec![PickerRow {
                    label: format!("(cannot list {path}: {e})"),
                    value: String::new(),
                    detail: None,
                    action: None,
                }],
                sel: 0,
                kind: PickerAction::View,
            }
        }
    };
    entries.sort_by_key(|e| {
        (
            !e.file_type().map(|t| t.is_dir()).unwrap_or(false),
            e.file_name().to_string_lossy().to_lowercase(),
        )
    });
    for e in entries.into_iter().take(400) {
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let ep = e.path().to_string_lossy().to_string();
        if is_dir {
            rows.push(PickerRow {
                label: format!("{}/", e.file_name().to_string_lossy()),
                value: format!("/files {ep}"),
                detail: None,
                action: Some(PickerAction::Command),
            });
        } else {
            rows.push(PickerRow {
                label: e.file_name().to_string_lossy().to_string(),
                value: ep.clone(),
                detail: Some(file_head(&ep, 400)),
                action: Some(PickerAction::View),
            });
        }
    }
    if rows.is_empty() {
        rows.push(PickerRow {
            label: "(empty directory)".into(),
            value: String::new(),
            detail: None,
            action: None,
        });
    }
    Picker {
        title: format!("files — {path}"),
        rows,
        sel: 0,
        kind: PickerAction::View,
    }
}

/// Run git and return stdout on success. Used by the Review picker only;
/// a real repo check is one `git` away under the cursor.
fn git_text(args: &[&str]) -> Option<String> {
    match std::process::Command::new("git").args(args).output() {
        Ok(o) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).to_string()),
        _ => None,
    }
}

/// Enter on a picked row: resolve its action and run it.
fn run_picker_selection(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    sel: usize,
) {
    let Some(row) = st.picker.as_ref().and_then(|p| p.rows.get(sel)).cloned() else {
        return;
    };
    let action = row.action.clone().unwrap_or_else(|| {
        st.picker
            .as_ref()
            .map(|p| p.kind.clone())
            .unwrap_or(PickerAction::View)
    });
    match action {
        PickerAction::SetModel => {
            if row.value.is_empty() {
                st.picker = None;
                return;
            }
            let a = cur(agent);
            let msg = match a.set_model(&row.value) {
                Ok(m) => m,
                Err(e) => format!("error: {e}"),
            };
            drop(a);
            st.push_msg_status(msg);
            st.picker = None;
        }
        PickerAction::Command => {
            st.picker = None;
            if row.value.trim().is_empty() {
                return;
            }
            let msg = local_command(st, agent, history, &row.value)
                .unwrap_or_else(|| format!("unknown command {}", row.value));
            st.push_msg_status(msg);
        }
        PickerAction::View => {
            let text = row.detail.unwrap_or(row.value);
            if text.trim().is_empty() {
                st.picker = None;
                return;
            }
            st.detail = Some(DetailView {
                title: row.label,
                text,
                scroll: 0,
            });
            st.picker = None;
        }
    }
}

#[derive(Clone)]
enum PaletteAction {
    NewSession,
    DeleteSession,
    Find,
    Links,
    Copy,
    CopyCode,
    ToggleSource,
    TogglePreview,
    ToggleLog,
    Help,
    Art,
    /// Jump straight to a session in the sidebar (the palette lists
    /// every session, so "jump" is filter-and-enter).
    SwitchTo(String),
    /// Run a shared slash command through the SAME path Enter does —
    /// `send_current` — so the palette and the composer cannot disagree
    /// about what a command means.
    Command(String),
    Theme(String),
    Bindings(veles_core::keymap::Keymap),
    Quit,
}

/// One palette row: what it reads as, what it does, and what it runs.
///
/// `desc` exists because the palette is the surface where you look for a
/// command you cannot remember the NAME of — a column of bare labels
/// answers only the case where you already knew it. Rows whose label is
/// a complete sentence on its own (a session, a theme) carry an empty
/// description rather than an invented one.
#[derive(Clone)]
struct PaletteEntry {
    label: String,
    desc: String,
    action: PaletteAction,
}

fn palette_entries(st: &UiState) -> Vec<PaletteEntry> {
    let entry = |label: &str, desc: &str, action: PaletteAction| PaletteEntry {
        label: label.to_string(),
        desc: desc.to_string(),
        action,
    };
    // The app-level actions only this surface can do.
    let mut v: Vec<PaletteEntry> = vec![
        entry(
            "new session",
            "start a fresh session",
            PaletteAction::NewSession,
        ),
        entry(
            "delete session",
            "delete the current session",
            PaletteAction::DeleteSession,
        ),
        entry(
            "find in transcript",
            "search this transcript",
            PaletteAction::Find,
        ),
        entry(
            "links in transcript",
            "open a link from this transcript",
            PaletteAction::Links,
        ),
        entry(
            "copy last reply",
            "put the last reply on the clipboard",
            PaletteAction::Copy,
        ),
        entry(
            "copy last code block",
            "put the last fenced block on the clipboard",
            PaletteAction::CopyCode,
        ),
        entry(
            "markdown source",
            "show the source instead of the rendering",
            PaletteAction::ToggleSource,
        ),
        entry(
            "composer preview",
            "preview the composer's markdown",
            PaletteAction::TogglePreview,
        ),
        entry(
            "engine log",
            "toggle the log pane",
            PaletteAction::ToggleLog,
        ),
        entry("help", "every key and command", PaletteAction::Help),
        entry("art", "the synthwave splash", PaletteAction::Art),
        entry(
            "quit",
            "leave (cancels a running turn first)",
            PaletteAction::Quit,
        ),
    ];

    // The shared command registry: every engine-state command the TUI
    // can run, one row each — /model, /memory, /skills, /plugins, /acl,
    // /checkpoints, /hooks and the rest. Sourced from `help_entries`
    // (the ONE help table, RM4.4), not a hand-maintained copy — INCLUDING
    // its description column, which used to be dropped here: the palette
    // rendered `/mech`, `/body`, `/failover` as bare names, so the one
    // place you go to find out what a command does was the one place it
    // was not written down.
    for (label, desc, head) in shared_command_palette() {
        v.push(entry(label, desc, PaletteAction::Command(head.to_string())));
    }

    // Sessions, so "jump" is type-name-and-enter. The message count is
    // the only thing that tells two similarly-named sessions apart.
    for s in &st.sessions {
        v.push(entry(
            &format!("session: {}", s.label),
            &format!("{} message(s)", s.count),
            PaletteAction::SwitchTo(s.id.clone()),
        ));
    }

    // Theme + bindings are surface-specific, same as /theme and
    // /bindings below the fold. Their labels are the whole story.
    let themes = ["synthwave", "classic", "amber", "mono"];
    for name in themes {
        v.push(entry(
            &format!("theme: {name}"),
            "",
            PaletteAction::Theme(name.into()),
        ));
    }
    for (name, km) in [
        ("normie", veles_core::keymap::Keymap::Normie),
        ("emacs", veles_core::keymap::Keymap::Emacs),
        ("vi", veles_core::keymap::Keymap::Vi),
    ] {
        v.push(entry(
            &format!("bindings: {name}"),
            "",
            PaletteAction::Bindings(km),
        ));
    }

    v
}

/// The palette's command rows, generated from the ONE help registry:
/// (label, description, head). `head` is the `/command` that
/// `send_current` dispatches; the description is what the row explains
/// itself with.
fn shared_command_palette() -> Vec<(&'static str, &'static str, &'static str)> {
    super::commands::help_entries(super::commands::Surface::Tui)
        .into_iter()
        .filter_map(|(label, desc)| {
            // The label is "/model [name]" — the head is the part the
            // dispatcher resolves. Argument hints stay in the label so
            // a palette still shows what an entry accepts.
            let head = label.split_whitespace().next().unwrap_or(label);
            // The shared dispatch and the TUI's app-level arms answer
            // every help row; a row whose head is not a slash command
            // would be a bug, not something to silently swallow.
            if head.starts_with('/') {
                Some((label, desc, head))
            } else {
                None
            }
        })
        .collect()
}

fn run_palette_action(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    action: PaletteAction,
) {
    match action {
        PaletteAction::NewSession => new_session_cmd(st, agent, history),
        PaletteAction::SwitchTo(id) => st.switch_to(agent, history, &id),
        PaletteAction::DeleteSession => delete_session_cmd(st),
        PaletteAction::Find => st.search = Some(SearchState::default()),
        PaletteAction::Links => {
            if st.all_links.is_empty() {
                st.push_msg_status(veles_core::i18n::lookup("no links in this transcript", &[]));
            } else {
                st.link_picker = Some(0);
            }
        }
        PaletteAction::Copy => copy_last_reply(st, false),
        PaletteAction::CopyCode => copy_last_reply(st, true),
        PaletteAction::ToggleSource => st.raw_view = !st.raw_view,
        PaletteAction::TogglePreview => st.show_preview = !st.show_preview,
        PaletteAction::ToggleLog => {
            st.show_log = !st.show_log;
            st.log_scroll = 0;
        }
        PaletteAction::Help => st.show_help = true,
        PaletteAction::Art => {
            let text = render_art_text(&st.theme);
            st.push_msg_status(text);
        }
        // Run the command exactly as typing it would: put it in the
        // composer and send. send_current resolves /quit, /new, /resume,
        // /retry, the shared dispatch and the surface arms, then echoes
        // the result into the transcript.
        PaletteAction::Command(head) => {
            st.input = head;
            st.caret = st.input.len();
            send_current(st, agent, history, ui_tx);
        }
        PaletteAction::Theme(name) => {
            match veles_core::theme::all()
                .into_iter()
                .find(|t| t.name == name)
            {
                Some(t) => {
                    st.theme = t;
                    st.push_msg_status(format!("theme \u{2192} {}", st.theme.name));
                }
                None => st.push_msg_status(veles_core::i18n::lookup(
                    "unknown theme '%{name}'",
                    &[("name", name.to_string())],
                )),
            }
        }
        PaletteAction::Bindings(k) => {
            st.keymap = k;
            st.vi_insert = true; // entering vi starts in insert
            st.push_msg_status(veles_core::i18n::lookup(
                "bindings %{name}",
                &[("name", k.name().to_string())],
            ));
        }
        PaletteAction::Quit => {
            // /quit and Ctrl-D both exit the app; the palette's quit
            // must too, instead of being the dead entry it used to be.
            st.quit = true;
        }
    }
}

// ── normie (current defaults) ────────────────────────────────────────

fn handle_normie(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    key: event::KeyEvent,
) -> bool {
    match key.code {
        KeyCode::F(1) => st.show_help = true,
        KeyCode::Char('y') | KeyCode::Char('Y')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            let want_code = matches!(key.code, KeyCode::Char('Y'))
                || key.modifiers.contains(KeyModifiers::SHIFT);
            copy_last_reply(st, want_code);
        }
        KeyCode::Char('f') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            st.search = Some(SearchState::default());
        }
        KeyCode::Char('o') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if st.all_links.is_empty() {
                st.push_msg_status(veles_core::i18n::lookup("no links in this transcript", &[]));
            } else {
                st.link_picker = Some(0);
            }
        }
        KeyCode::F(3) => {
            st.raw_view = !st.raw_view;
            st.push_msg_status(
                if st.raw_view {
                    "markdown source (F3 to render)"
                } else {
                    "rendered markdown (F3 for source)"
                }
                .into(),
            );
        }
        KeyCode::F(4) => {
            st.show_preview = !st.show_preview;
        }
        KeyCode::F(2) => {
            st.show_log = !st.show_log;
            st.log_scroll = 0;
        }
        KeyCode::PageUp if st.show_log && key.modifiers.contains(KeyModifiers::SHIFT) => {
            st.log_scroll = st.log_scroll.saturating_add(5);
        }
        KeyCode::PageDown if st.show_log && key.modifiers.contains(KeyModifiers::SHIFT) => {
            st.log_scroll = st.log_scroll.saturating_sub(5);
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            return interrupt_or_quit(st);
        }
        // Ctrl-Z: suspend to the shell, not "type the letter z".
        KeyCode::Char('z') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Err(e) = suspend_terminal() {
                st.push_msg_status(veles_core::i18n::lookup(
                    "suspend failed: %{e}",
                    &[("e", e.to_string())],
                ));
            }
            return false;
        }
        // Ctrl-/ (or Ctrl-_): undo the last composer edit.
        KeyCode::Char(c)
            if key.modifiers.contains(KeyModifiers::CONTROL) && (c == '/' || c == '_') =>
        {
            composer_undo(st);
            return false;
        }
        KeyCode::Esc => {
            if st.busy {
                if let Some(c) = &st.cancel {
                    c.cancel();
                }
                return false;
            }
            return true;
        }
        // Alt-x (M-x) opens the command palette in every mode.
        KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::ALT) => {
            st.palette = Some(PaletteState::default());
        }
        // Ctrl-K: like M-x, but reachable without a meta key.
        KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            st.palette = Some(PaletteState::default());
        }
        // Newline instead of sending: Alt-Enter, Shift-Enter (where
        // the terminal distinguishes it) or Ctrl-Enter — one contract
        // across all three keymaps so the composer behaves the same
        // everywhere.
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::ALT)
                || key.modifiers.contains(KeyModifiers::SHIFT)
                || key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            insert_newline(st);
        }
        KeyCode::Enter => send_current(st, agent, history, ui_tx),
        // Up/Down walk the lines of a multiline input first; history
        // waits at the edges (readline contract).
        KeyCode::Up => up_from(st),
        KeyCode::Down => down_from(st),
        KeyCode::Left => move_left(st),
        KeyCode::Right => move_right(st),
        KeyCode::Home => home(st),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => st.scrollback = 0,
        KeyCode::End => end(st),
        KeyCode::Backspace => backspace(st),
        KeyCode::Delete => del_char(st),
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => clear_input(st),
        KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) => kill_word_bwd(st),
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            new_session_cmd(st, agent, history);
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                cycle_session(st, agent, history, -1);
            } else {
                cycle_session(st, agent, history, 1);
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            delete_session_cmd(st);
        }
        KeyCode::PageUp => st.scrollback = st.scrollback.saturating_add(10),
        KeyCode::PageDown => st.scrollback = st.scrollback.saturating_sub(10),
        KeyCode::Char(c)
            if !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT) =>
        {
            composer_insert(st, &c.to_string());
        }
        _ => {}
    }
    false
}

// ── emacs ────────────────────────────────────────────────────────────

fn handle_emacs(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    key: event::KeyEvent,
) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::F(1) => st.show_help = true,
        KeyCode::F(2) => {
            st.show_log = !st.show_log;
            st.log_scroll = 0;
        }
        KeyCode::F(3) => {
            st.raw_view = !st.raw_view;
            st.push_msg_status(
                if st.raw_view {
                    "markdown source (F3 to render)"
                } else {
                    "rendered markdown (F3 for source)"
                }
                .into(),
            );
        }
        KeyCode::F(4) => {
            st.show_preview = !st.show_preview;
        }
        // C-g: cancel (like Esc).
        KeyCode::Char('g') if ctrl => {
            if st.busy {
                if let Some(c) = &st.cancel {
                    c.cancel();
                }
                return false;
            }
            return true;
        }
        KeyCode::Char('c') if ctrl => return interrupt_or_quit(st),
        // C-z is "suspend" in emacs too, and C-/ is undo, exactly where
        // an emacs user reaches for them.
        KeyCode::Char('z') if ctrl => {
            if let Err(e) = suspend_terminal() {
                st.push_msg_status(veles_core::i18n::lookup(
                    "suspend failed: %{e}",
                    &[("e", e.to_string())],
                ));
            }
            return false;
        }
        KeyCode::Char(c) if ctrl && (c == '/' || c == '_') => {
            composer_undo(st);
            return false;
        }
        KeyCode::Esc => {
            if st.busy {
                if let Some(c) = &st.cancel {
                    c.cancel();
                }
                return false;
            }
            return true;
        }
        // M-x: command palette.
        KeyCode::Char('x') if alt => {
            st.palette = Some(PaletteState::default());
        }
        // C-j: newline instead of send (the emacs way) — and the same
        // Alt/Shift-Enter contract the other keymaps have.
        KeyCode::Enter if ctrl || alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
            insert_newline(st)
        }
        KeyCode::Enter => send_current(st, agent, history, ui_tx),
        // Editing chords.
        KeyCode::Char('a') if ctrl => home(st),
        KeyCode::Char('e') if ctrl => end(st),
        KeyCode::Char('b') if ctrl => move_left(st),
        KeyCode::Char('f') if ctrl => move_right(st),
        KeyCode::Char('b') if alt => word_left(st),
        KeyCode::Char('f') if alt => word_right(st),
        KeyCode::Char('p') if ctrl => hist_prev(st),
        KeyCode::Char('n') if ctrl => hist_next(st),
        KeyCode::Char('k') if ctrl => kill_to_eol(st),
        KeyCode::Char('y') if ctrl => yank(st),
        KeyCode::Char('u') if ctrl => clear_input(st),
        KeyCode::Char('w') if ctrl => kill_word_bwd(st),
        KeyCode::Char('d') if ctrl => del_char(st),
        KeyCode::Char('h') if ctrl => backspace(st),
        // App shortcuts move to the M-* prefix in emacs mode so the
        // C-* chord stays emacs.
        KeyCode::Char('f') if alt => st.search = Some(SearchState::default()),
        KeyCode::Char('o') if alt => {
            if st.all_links.is_empty() {
                st.push_msg_status(veles_core::i18n::lookup("no links in this transcript", &[]));
            } else {
                st.link_picker = Some(0);
            }
        }
        KeyCode::Char('n') if alt => new_session_cmd(st, agent, history),
        KeyCode::Char('d') if alt => delete_session_cmd(st),
        KeyCode::Char('y') if alt => copy_last_reply(st, false),
        KeyCode::Char('Y') if alt => copy_last_reply(st, true),
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                cycle_session(st, agent, history, -1);
            } else {
                cycle_session(st, agent, history, 1);
            }
        }
        KeyCode::PageUp if st.show_log && key.modifiers.contains(KeyModifiers::SHIFT) => {
            st.log_scroll = st.log_scroll.saturating_add(5);
        }
        KeyCode::PageDown if st.show_log && key.modifiers.contains(KeyModifiers::SHIFT) => {
            st.log_scroll = st.log_scroll.saturating_sub(5);
        }
        KeyCode::PageUp => st.scrollback = st.scrollback.saturating_add(10),
        KeyCode::PageDown => st.scrollback = st.scrollback.saturating_sub(10),
        // Arrows are still fine.
        KeyCode::Up => up_from(st),
        KeyCode::Down => down_from(st),
        KeyCode::Left => move_left(st),
        KeyCode::Right => move_right(st),
        KeyCode::Home => home(st),
        KeyCode::End => end(st),
        KeyCode::Backspace => backspace(st),
        KeyCode::Delete => del_char(st),
        KeyCode::Char(c) if !ctrl && !alt => {
            composer_insert(st, &c.to_string());
        }
        _ => {}
    }
    false
}

// ── vi ───────────────────────────────────────────────────────────────

fn handle_vi(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    ui_tx: &mpsc::Sender<UiEvent>,
    key: event::KeyEvent,
) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // App-level keys work in both modes.
    match key.code {
        KeyCode::F(1) => st.show_help = true,
        KeyCode::F(2) => {
            st.show_log = !st.show_log;
            st.log_scroll = 0;
        }
        KeyCode::F(3) => {
            st.raw_view = !st.raw_view;
        }
        KeyCode::F(4) => {
            st.show_preview = !st.show_preview;
        }
        KeyCode::Char('c') if ctrl => return interrupt_or_quit(st),
        KeyCode::Char('z') if ctrl => {
            if let Err(e) = suspend_terminal() {
                st.push_msg_status(veles_core::i18n::lookup(
                    "suspend failed: %{e}",
                    &[("e", e.to_string())],
                ));
            }
            return false;
        }
        KeyCode::Char(c) if ctrl && (c == '/' || c == '_') => {
            composer_undo(st);
            return false;
        }
        KeyCode::Char('x') if alt => {
            st.palette = Some(PaletteState::default());
            return false;
        }
        KeyCode::Tab => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                cycle_session(st, agent, history, -1);
            } else {
                cycle_session(st, agent, history, 1);
            }
            return false;
        }
        KeyCode::PageUp => st.scrollback = st.scrollback.saturating_add(10),
        KeyCode::PageDown => st.scrollback = st.scrollback.saturating_sub(10),
        _ => {}
    }

    if st.vi_insert {
        match key.code {
            KeyCode::Esc => {
                st.vi_insert = false;
                // caret one left, vim-style
                if st.caret > 0 {
                    st.caret = prev_boundary(&st.input, st.caret);
                }
            }
            KeyCode::Enter if ctrl || alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                insert_newline(st)
            }
            KeyCode::Enter => send_current(st, agent, history, ui_tx),
            KeyCode::Backspace => backspace(st),
            KeyCode::Delete => del_char(st),
            KeyCode::Left => move_left(st),
            KeyCode::Right => move_right(st),
            KeyCode::Up => up_from(st),
            KeyCode::Down => down_from(st),
            KeyCode::Home => home(st),
            KeyCode::End => end(st),
            KeyCode::Char(c) if !ctrl && !alt => {
                composer_insert(st, &c.to_string());
            }
            _ => {}
        }
    } else {
        // normal (command) mode
        match key.code {
            KeyCode::Esc => {}
            KeyCode::Enter => send_current(st, agent, history, ui_tx),
            KeyCode::Char(':') => st.palette = Some(PaletteState::default()),
            KeyCode::Char('i') => {
                st.vi_insert = true;
            }
            KeyCode::Char('a') => {
                if st.caret < st.input.len() {
                    st.caret = next_boundary(&st.input, st.caret);
                }
                st.vi_insert = true;
            }
            KeyCode::Char('A') => {
                st.caret = st.input.len();
                st.vi_insert = true;
            }
            KeyCode::Char('I') => {
                st.caret = 0;
                st.vi_insert = true;
            }
            KeyCode::Char('h') => move_left(st),
            KeyCode::Char('l') => move_right(st),
            KeyCode::Char('j') => hist_next(st),
            KeyCode::Char('k') => hist_prev(st),
            KeyCode::Char('0') => home(st),
            KeyCode::Char('$') => end(st),
            KeyCode::Char('x') => {
                if st.caret < st.input.len() {
                    let b = next_boundary(&st.input, st.caret);
                    st.yank = st.input[st.caret..b].to_string();
                    st.input.replace_range(st.caret..b, "");
                }
            }
            KeyCode::Char('d') => {
                // dd: delete whole line into the kill ring.
                st.yank = st.input.clone();
                clear_input(st);
            }
            KeyCode::Char('D') => kill_to_eol(st),
            KeyCode::Char('p') => yank(st),
            KeyCode::Char('P') => {
                let saved = st.caret;
                yank(st);
                st.caret = saved;
            }
            _ => {}
        }
    }
    false
}

fn handle_mouse(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    _ui_tx: &mpsc::Sender<UiEvent>,
    ev: event::MouseEvent,
) {
    match ev.kind {
        MouseEventKind::ScrollUp => st.scrollback = st.scrollback.saturating_add(3),
        MouseEventKind::ScrollDown => st.scrollback = st.scrollback.saturating_sub(3),
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            // A link under the pointer wins over everything else.
            if let Some((_, _, _, url)) = st
                .link_hits
                .iter()
                .find(|(r, cs, ce, _)| *r == ev.row && ev.column >= *cs && ev.column < *ce)
                .cloned()
            {
                match open_url(&url) {
                    Ok(()) => st.push_msg_status(veles_core::i18n::lookup(
                        "opened %{url}",
                        &[("url", url.to_string())],
                    )),
                    Err(e) => st.push_msg_status(veles_core::i18n::lookup(
                        "could not open %{url}: %{e}",
                        &[("url", url.to_string()), ("e", e.to_string())],
                    )),
                }
                return;
            }
            // Click on a reasoning header toggles its expansion.
            if let Some((_, idx)) = st
                .reasoning_header_rows
                .iter()
                .find(|(r, _)| *r == ev.row)
                .cloned()
            {
                if let Some(b) = st.transcript.blocks.get_mut(idx) {
                    if b.kind == Kind::Reasoning {
                        b.collapsed = !b.collapsed;
                        // The cached render was built for the other
                        // state; the staleness key cannot see this.
                        st.cache.invalidate(idx);
                    }
                }
                return;
            }
            // Sidebar clicks switch sessions; anything else focuses the
            // composer. Coordinates are cell-based; the draw pass knows
            // the exact areas, so compute from the last layout.
            if let Some(area) = st.sidebar_area {
                if ev.row > area.y
                    && ev.row < area.y + area.height - 1
                    && ev.column > area.x
                    && ev.column < area.x + area.width - 1
                {
                    let idx = (ev.row - area.y - 1) as usize;
                    if idx < st.sessions.len() && !st.busy {
                        st.sel = idx;
                        let next = st.sessions[idx].id.clone();
                        if next != st.session {
                            st.switch_to(agent, history, &next);
                        }
                    }
                    return;
                }
            }
            if let Some(area) = st.input_area {
                // Any row of the composer box maps to its LINE
                // (scroll-adjusted); the column maps to a caret by
                // DISPLAY width, not char count — wide glyphs must
                // land the caret where the eye clicked.
                if ev.row > area.y
                    && ev.row < area.y + area.height - 1
                    && ev.column > area.x
                    && ev.column < area.x + area.width - 1
                {
                    use unicode_width::UnicodeWidthChar;
                    let row = (ev.row - area.y - 1 + st.input_vscroll) as usize;
                    let (s, e) = line_span(&st.input, row);
                    let want = (ev.column - area.x - 1 - 2) as usize; // past "❯ "
                    let mut seen = 0usize;
                    let mut caret = e;
                    for (i, c) in st.input[s..e].char_indices() {
                        if seen >= want {
                            caret = s + i;
                            break;
                        }
                        seen += UnicodeWidthChar::width(c).unwrap_or(1);
                    }
                    st.caret = caret;
                }
            }
        }
        _ => {}
    }
}

// There is no `#[allow(clippy::await_holding_lock)]` here any more, and
// that is the whole point: the turn clones the `Arc<Agent>` out of the
// handle and runs on `&self`, so no guard spans the await and nothing a
// user types mid-turn has to queue behind it.
fn spawn_turn(
    agent: AgentHandle,
    history: Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    text: String,
    tx: mpsc::Sender<UiEvent>,
    cancel: veles_core::providers::CancelToken,
) {
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(UiEvent::TurnEnd {
                    text: format!("[error] runtime: {e}"),
                    route: String::new(),
                    model: String::new(),
                    ok: false,
                    interrupted: false,
                    prompt_tokens: 0,
                    completion_tokens: 0,
                    cached_tokens: None,
                    reasoning_tokens: None,
                });
                return;
            }
        };
        let mut route = String::new();
        let model = String::new();
        let mut pt = 0u64;
        let mut ct = 0u64;
        let res = rt.block_on(async {
            let mut h = history.lock_ok().clone();
            let a = cur(&agent);
            let out = {
                a.run_turn(&mut h, &text, Some(cancel), |ev| match ev {
                    StreamEvent::Route {
                        provider, model, ..
                    } => route = format!("{provider}/{model}"),
                    StreamEvent::Delta(t) => {
                        let _ = tx.send(UiEvent::Delta(t.clone()));
                    }
                    StreamEvent::ReasoningDelta(t) => {
                        let _ = tx.send(UiEvent::ReasoningDelta(t.clone()));
                    }
                    StreamEvent::ToolStart(name, args) => {
                        let _ = tx.send(UiEvent::ToolStart(name.clone(), args.clone()));
                    }
                    StreamEvent::ToolEnd(name, result) => {
                        let _ = tx.send(UiEvent::ToolEnd(name.clone(), result.clone()));
                    }
                    StreamEvent::Note(n) => {
                        let _ = tx.send(UiEvent::Note(Box::new(n.clone())));
                    }
                    _ => {}
                })
                .await
            };
            *history.lock_ok() = h;
            out
        });
        let ev = match res {
            Ok(tr) => {
                pt = tr.prompt_tokens;
                ct = tr.completion_tokens;
                // A reasoning-only completion used to render an empty
                // bubble here — the REPL and desktop say what happened
                // via empty_reply_notice, so the TUI says it too.
                let body = if tr.reply.trim().is_empty() {
                    tr.empty_reply_notice().unwrap_or_else(|| tr.reply.clone())
                } else {
                    tr.reply.clone()
                };
                UiEvent::TurnEnd {
                    // MEDIA: markers become path notes (text surface).
                    text: veles_core::media::render_media_notes(&body),
                    route,
                    model: tr.model,
                    ok: true,
                    interrupted: tr.interrupted,
                    prompt_tokens: pt,
                    completion_tokens: ct,
                    cached_tokens: tr.cached_tokens,
                    reasoning_tokens: tr.reasoning_tokens,
                }
            }
            Err(e) => UiEvent::TurnEnd {
                text: format!("[error] {e:#}"),
                route,
                model,
                ok: false,
                interrupted: false,
                prompt_tokens: pt,
                completion_tokens: ct,
                cached_tokens: None,
                reasoning_tokens: None,
            },
        };
        let _ = tx.send(ev);
    });
}

// ── drawing ─────────────────────────────────────────────────────────

/// The header clock: to the minute, because it only moves on a redraw.
fn now() -> String {
    Local::now().format("%H:%M").to_string()
}

/// A transcript entry's stamp: to the second, like every other block
/// (the shared transcript's `now_hms`).
fn now_hms() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn draw(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, st: &mut UiState) -> Result<()> {
    terminal.draw(|f| {
        let size = f.size();
        // Optional panes make the vertical split dynamic: the log pane
        // (F2) sits between the body and the composer.
        // Optional panes need a terminal tall enough to still leave a
        // usable transcript: title(1) + body + input(3) + status(1).
        let roomy = size.height >= 16;
        let log_h: u16 = if st.show_log && roomy {
            (size.height / 3).clamp(5, 14)
        } else {
            0
        };
        // Preview height follows the content, within reason.
        let preview_h: u16 = if st.show_preview && roomy {
            let n = st.input.lines().count().max(1) as u16;
            (n + 2).clamp(4, 10)
        } else {
            0
        };
        let mut cons: Vec<Constraint> = Vec::with_capacity(6);
        cons.push(Constraint::Length(1)); // title bar
        cons.push(Constraint::Min(1)); // body
        if log_h > 0 {
            cons.push(Constraint::Length(log_h));
        }
        if preview_h > 0 {
            cons.push(Constraint::Length(preview_h));
        }
        let search_h: u16 = if st.search.is_some() { 1 } else { 0 };
        if search_h > 0 {
            cons.push(Constraint::Length(search_h));
        }
        // The composer grows with its content (multiline input),
        // capped at half the terminal so the transcript never
        // collapses; past the cap the input scrolls internally.
        let input_h = composer_height(st.input.lines().count().max(1), size.height);
        cons.push(Constraint::Length(input_h)); // input
        cons.push(Constraint::Length(1)); // status
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(cons)
            .split(size);
        let mut ci = 0usize;
        let title_area = chunks[ci];
        ci += 1;
        let body = chunks[ci];
        ci += 1;
        let log_area = if log_h > 0 {
            let a = chunks[ci];
            ci += 1;
            Some(a)
        } else {
            None
        };
        let preview_area = if preview_h > 0 {
            let a = chunks[ci];
            ci += 1;
            Some(a)
        } else {
            None
        };
        let search_area = if search_h > 0 {
            let a = chunks[ci];
            ci += 1;
            Some(a)
        } else {
            None
        };
        let input_area = chunks[ci];
        ci += 1;
        let status_area = chunks[ci];

        draw_title(f, title_area, &st.theme);
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(26), Constraint::Min(1)])
            .split(body);
        draw_sidebar(f, body_chunks[0], st);
        let chat_area = body_chunks[1];
        draw_chat(f, chat_area, st);
        if let Some(a) = log_area {
            draw_log(f, a, st);
        }
        if let Some(a) = preview_area {
            draw_preview(f, a, st);
        }
        if let (Some(a), Some(s)) = (search_area, st.search.clone()) {
            draw_search(f, a, st, &s);
        }
        draw_input(f, input_area, st);
        draw_status(f, status_area, st);

        if let Some(req) = &st.pending_ask {
            draw_ask(
                f,
                req,
                &st.ask_input,
                st.ask_note_mode,
                st.ask_scroll,
                st.ask_sel,
                &st.ask_toggles,
                &st.ask_other,
                &st.theme,
            );
        }
        if st.show_help {
            draw_help(f, &st.theme);
        }
        if let Some(sid) = &st.confirm_delete {
            draw_confirm(f, sid, &st.theme);
        }
        if let Some(sel) = st.link_picker {
            draw_link_picker(f, st, sel);
        }
        if st.palette.is_some() {
            draw_palette(f, st, &st.theme);
        }
        if st.picker.is_some() {
            draw_picker(f, st, &st.theme);
        }
        if st.detail.is_some() {
            draw_detail(f, st, &st.theme);
        }
        // Cursor lives in the composer (or the ask input). All of this
        // is saturating: a squeezed layout (tiny terminal, or the log
        // and preview panes both open) can hand back a zero-sized rect,
        // and `area.x + area.width - 2` then underflows — a real panic,
        // caught by the PTY smoke test.
        if st.pending_ask.is_none()
            && st.confirm_delete.is_none()
            && !st.show_help
            && st.palette.is_none()
            && st.picker.is_none()
            && st.detail.is_none()
        {
            if let Some(area) = st.input_area.filter(|a| a.width >= 4 && a.height >= 2) {
                // Multiline: row from the caret's line (minus the
                // scroll), column from the caret's display width in
                // that line (minus the horizontal offset — the same
                // two numbers draw_input scrolled by, so the cursor
                // and the text can't disagree).
                let lines: Vec<&str> = st.input.split('\n').collect();
                let (crow, ccol) = caret_line_col(&st.input, st.caret);
                let cl = lines.get(crow).copied().unwrap_or("");
                let colw = UnicodeWidthStr::width(&cl[..ccol.min(cl.len())]);
                let text_w = (area.width.saturating_sub(3)) as usize; // border + "❯ "
                let off = hscroll_offset(colw, text_w);
                let vis_row = crow.saturating_sub(st.input_vscroll as usize) as u16;
                let x = area
                    .x
                    .saturating_add(3)
                    .saturating_add((colw - off).min(text_w) as u16);
                let max_x = area.x.saturating_add(area.width).saturating_sub(2);
                let y = area.y.saturating_add(1).saturating_add(vis_row);
                let max_y = area.y.saturating_add(area.height).saturating_sub(2);
                f.set_cursor(x.min(max_x), y.min(max_y));
            }
        }
    })?;
    Ok(())
}

/// Put text on the system clipboard with OSC 52.
///
/// crossterm has no clipboard support and there is no clipboard crate in
/// the tree, so this writes the escape sequence itself. The upside is
/// that it works over ssh and inside multiplexers that pass OSC through
/// — the terminal does the copying, not the host.
fn osc52_copy(text: &str) -> std::io::Result<()> {
    use base64::Engine;
    use std::io::Write;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = io::stdout();
    // ESC ] 52 ; c ; <base64> BEL
    write!(out, "\x1b]52;c;{b64}\x07")?;
    out.flush()
}

/// The last fenced code block in a reply, or the whole text when there
/// is no fence.
fn last_code_block(text: &str) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            match cur.take() {
                Some(b) => blocks.push(b.join("\n")),
                None => cur = Some(Vec::new()),
            }
            continue;
        }
        if let Some(b) = cur.as_mut() {
            b.push(line.to_string());
        }
    }
    blocks.pop()
}

/// Hand a URL to the desktop browser. Mirrors the Tcl side's opener
/// chain; the TUI has no browser of its own and shelling out is the
/// whole mechanism.
/// Policy half of `open_url`, kept separate so it can be tested without
/// launching anything. Nothing in the test suite may spawn a browser.
fn url_is_openable(url: &str) -> std::io::Result<()> {
    // Only ever launch http(s): a transcript is untrusted text, and
    // handing `file://` or a custom scheme to xdg-open is a footgun.
    if url.starts_with("http://") || url.starts_with("https://") {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "only http(s) links are opened",
        ))
    }
}

fn open_url(url: &str) -> std::io::Result<()> {
    url_is_openable(url)?;
    veles_core::clipboard::open_url(url)
}

/// Find bar (Ctrl-F) — one line above the status bar.
fn draw_search(f: &mut ratatui::Frame<'_>, area: Rect, st: &UiState, s: &SearchState) {
    let t = &st.theme;
    let hits = if s.query.is_empty() {
        "type to search".to_string()
    } else if s.hits.is_empty() {
        "no matches".to_string()
    } else {
        format!("{}/{}", s.at + 1, s.hits.len())
    };
    let line = Line::from(vec![
        Span::styled(" find ", Style::default().fg(t.bg()).bg(t.cyan())),
        Span::styled(format!(" {}", s.query), Style::default().fg(t.text())),
        Span::styled("▏", Style::default().fg(t.pink())),
        Span::styled(format!("  {hits}"), Style::default().fg(t.dim())),
        Span::styled("   ↑/↓ or Enter · Esc closes", Style::default().fg(t.dim())),
    ]);
    f.render_widget(
        Paragraph::new(line).style(Style::default().bg(t.panel())),
        area,
    );
}

/// Link picker (Ctrl-O) — every URL currently on screen, numbered.
fn draw_link_picker(f: &mut ratatui::Frame<'_>, st: &UiState, sel: usize) {
    let t = &st.theme;
    let area = centered_rect(70, 50, f.size());
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        " links in view — Enter opens · 1-9 jumps · Esc closes",
        Style::default().fg(t.dim()),
    ))];
    for (i, url) in st.all_links.iter().enumerate() {
        let marked = i == sel;
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}{} ", if marked { "▸" } else { " " }, i + 1),
                Style::default().fg(if marked { t.pink() } else { t.dim() }),
            ),
            Span::styled(
                truncate(url, area.width.saturating_sub(8) as usize),
                Style::default().fg(if marked { t.cyan() } else { t.text() }),
            ),
        ]));
    }
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.cyan()))
            .title(Span::styled(" open link ", Style::default().fg(t.cyan()))),
    );
    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(p, area);
}

/// Live markdown preview of the composer (F4).
///
/// A separate pane on purpose: the caret column is computed from the
/// RAW input string, so rendering markdown in place (where `**bold**`
/// becomes `bold`) would desync the cursor by exactly the number of
/// markup characters to its left.
fn draw_preview(f: &mut ratatui::Frame<'_>, area: Rect, st: &UiState) {
    let t = &st.theme;
    let inner_w = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    if st.input.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            " preview — type markdown above",
            Style::default().fg(t.dim()),
        )));
    } else {
        for l in md_lines(&st.input, t, inner_w) {
            lines.extend(wrap_line(Line::from(l), inner_w));
        }
    }
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.hair()))
            .title(Span::styled(
                " preview (F4) ",
                Style::default().fg(t.purple()),
            )),
    );
    f.render_widget(p, area);
}

/// Engine log pane (F2) — the destination for everything `tracing`
/// used to dump onto stdout, straight through the middle of the UI.
fn draw_log(f: &mut ratatui::Frame<'_>, area: Rect, st: &UiState) {
    let t = &st.theme;
    let inner_h = area.height.saturating_sub(2) as usize;
    // Only what the pane can show, plus however far it is scrolled —
    // cloning the whole 500-line ring every frame is pure waste.
    let total = veles_core::logbuf::len();
    let want = inner_h + st.log_scroll as usize;
    let all = veles_core::logbuf::snapshot(want);
    let off = all.len().saturating_sub(inner_h + st.log_scroll as usize);
    let lines: Vec<Line<'static>> = all
        .iter()
        .skip(off)
        .take(inner_h)
        .map(|l| {
            let col = match l.level.as_str() {
                "ERROR" => t.red(),
                "WARN" => t.gold(),
                "INFO" => t.text(),
                _ => t.dim(),
            };
            Line::from(vec![
                Span::styled(format!("{} ", l.time), Style::default().fg(t.dim())),
                Span::styled(format!("{:<5} ", l.level), Style::default().fg(col)),
                Span::styled(
                    format!("{} ", short_target(&l.target)),
                    Style::default().fg(t.purple()),
                ),
                Span::styled(l.msg.clone(), Style::default().fg(col)),
            ])
        })
        .collect();
    let title = if total == 0 {
        " log — quiet ".to_string()
    } else {
        format!(
            " log — {total} lines · {} ",
            veles_core::logbuf::log_path().display()
        )
    };
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.hair()))
            .title(Span::styled(
                truncate(&title, area.width.saturating_sub(4) as usize),
                Style::default().fg(t.gold()),
            )),
    );
    f.render_widget(p, area);
}

/// `veles_core::frontends::gateway` → `gateway`: the module tail is the
/// useful part in a narrow pane.
fn short_target(target: &str) -> String {
    target.rsplit("::").next().unwrap_or(target).to_string()
}

fn draw_title(f: &mut ratatui::Frame<'_>, area: Rect, t: &veles_core::theme::Theme) {
    f.render_widget(Block::default().style(Style::default().bg(t.bg())), area);
    // "▲ VELES" — gradient pink → purple → cyan across the wordmark.
    let word: Vec<Span> = "VELES"
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let col = match i {
                0 | 1 => t.pink(),
                2 | 3 => t.purple(),
                _ => t.cyan(),
            };
            Span::styled(
                c.to_string(),
                Style::default().fg(col).add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    let mut line = vec![Span::styled(
        "▲ ",
        Style::default().fg(t.pink()).add_modifier(Modifier::BOLD),
    )];
    line.extend(word);
    line.push(Span::styled(
        " · neon impressionism",
        Style::default().fg(t.dim()),
    ));
    let clock = now().to_string();
    let clock_w = clock.width() as u16;
    let line_w = line.iter().map(|s| s.content.width()).sum::<usize>() as u16;
    line.push(Span::raw(
        " ".repeat(area.width.saturating_sub(line_w + clock_w + 1) as usize),
    ));
    line.push(Span::styled(clock, Style::default().fg(t.dim())));
    f.render_widget(Paragraph::new(Line::from(line)), area);
}

fn draw_sidebar(f: &mut ratatui::Frame<'_>, area: Rect, st: &mut UiState) {
    let t = &st.theme;
    st.sidebar_area = Some(area);
    let items: Vec<ListItem> = st
        .sessions
        .iter()
        .map(|s| {
            let active = s.id == st.session;
            let label = truncate(&s.label, 15);
            let text = if active {
                format!("◆ {label} · {}", s.count)
            } else {
                format!("  {label} · {}", s.count)
            };
            let style = if active {
                Style::default().fg(t.pink()).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.text())
            };
            ListItem::new(Line::from(Span::styled(text, style)))
        })
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.hair()))
                .title(Span::styled(
                    " sessions ",
                    Style::default().fg(t.purple()).add_modifier(Modifier::BOLD),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(t.panel_hi())
                .fg(t.cyan())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    if !st.sessions.is_empty() {
        state.select(Some(st.sel));
    }
    f.render_stateful_widget(list, area, &mut state);
}

/// Render ONE transcript block into wrapped lines + link regions.
/// Called only when that block changed (see `MsgRender`).
fn render_msg(
    m: &veles_core::transcript::Block,
    t: &veles_core::theme::Theme,
    wrap_w: usize,
    raw_view: bool,
) -> MsgRender {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut links: Vec<(usize, usize, usize, String)> = Vec::new();
    match m.kind {
        Kind::User => {
            // An injected row is the engine talking, not the human —
            // it gets the ⇒ label (RM3-S5) so it can never read as
            // something the user typed.
            let (head, head_style) = match &m.injection {
                Some(inj) => {
                    let label = if let Some(surf) = inj.class.strip_prefix("origin:") {
                        format!("⇒ origin: {surf} ▸ ")
                    } else if let Some(from) = inj.class.strip_prefix("mail:") {
                        // Another session's words, not the human's.
                        format!("✉ mail from {from} ▸ ")
                    } else {
                        format!("⇒ {} ▸ ", inj.class)
                    };
                    (label, Style::default().fg(t.gold()))
                }
                None => (
                    format!("{} you ▸ ", m.time),
                    Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
                ),
            };
            let logical = Line::from(vec![
                Span::styled(head, head_style),
                Span::styled(m.body.clone(), Style::default().fg(t.text())),
            ]);
            lines.extend(wrap_line(logical, wrap_w));
        }
        Kind::Reply if raw_view => {
            // View source (F3): the markdown exactly as it arrived.
            for (i, raw) in m.body.lines().enumerate() {
                let head = if i == 0 {
                    format!("{} veles ▸ ", m.time)
                } else {
                    "          ".to_string()
                };
                let logical = Line::from(vec![
                    Span::styled(head, Style::default().fg(t.pink())),
                    Span::styled(raw.to_string(), Style::default().fg(t.dim())),
                ]);
                lines.extend(wrap_line(logical, wrap_w));
            }
        }
        Kind::Reply => {
            for (i, (l, link_spans)) in md_lines_tracked(&m.body, t, wrap_w.saturating_sub(10))
                .into_iter()
                .enumerate()
            {
                let mut spans = Vec::new();
                if i == 0 {
                    spans.push(Span::styled(
                        format!("{} veles ▸ ", m.time),
                        Style::default().fg(t.pink()).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled("          ", Style::default().fg(t.dim())));
                }
                let head_w: usize =
                    unicode_width::UnicodeWidthStr::width(spans[0].content.as_ref());
                spans.extend(l);
                for (row_line, src) in wrap_line_tracked(Line::from(spans), wrap_w) {
                    let row_idx = lines.len();
                    for ls in &link_spans {
                        let s = ls.start + head_w;
                        let e = ls.end + head_w;
                        let vis_s = s.max(src.start);
                        let vis_e = e.min(src.end);
                        if vis_s < vis_e {
                            links.push((
                                row_idx,
                                vis_s - src.start,
                                vis_e - src.start,
                                ls.url.clone(),
                            ));
                        }
                    }
                    lines.push(row_line);
                }
            }
        }
        Kind::Reasoning => {
            // Collapsed: a one-line header (nothing vanishes — R4).
            // Expanded: the full text underneath.
            let icon = if m.collapsed { "▶" } else { "▼" };
            let header = if m.body.is_empty() {
                "thinking…".to_string()
            } else {
                format!("{} lines", m.body.lines().count().max(1))
            };
            let logical = Line::from(vec![
                Span::styled(format!("{icon} "), Style::default().fg(t.purple())),
                Span::styled(
                    format!("⟦ reasoning — {header} ⟧"),
                    Style::default()
                        .fg(t.purple())
                        .add_modifier(Modifier::ITALIC),
                ),
            ]);
            lines.extend(wrap_line(logical, wrap_w));
            if !m.collapsed {
                for raw_line in m.body.lines() {
                    let l = Line::from(vec![Span::styled(
                        format!("  {raw_line}"),
                        Style::default().fg(t.dim()),
                    )]);
                    lines.extend(wrap_line(l, wrap_w));
                }
            }
        }
        Kind::Tool => {
            // ⚙ name + args on one line, the result indented beneath —
            // the same shape the desktop renders (chat.tcl tool
            // rounds). `args` was flattened to one line BY CONSTRUCTION
            // (transcript.rs `one_line`), so the header cannot wrap
            // file fragments into the ⚙ line.
            if let Some(tool) = &m.tool {
                let mut head = format!("⚙ {}", tool.name);
                if !tool.args.is_empty() {
                    let shown = truncate(&tool.args, 60);
                    head.push_str(&format!(" {shown}"));
                }
                match &tool.result {
                    None => head.push_str(" …"),
                    Some(r) if r.trim().is_empty() => head.push_str(" ✓"),
                    Some(_) => {}
                }
                let logical = Line::from(vec![Span::styled(head, Style::default().fg(t.gold()))]);
                lines.extend(wrap_line(logical, wrap_w));
                if let Some(r) = tool.result.as_ref().filter(|r| !r.trim().is_empty()) {
                    for raw_line in r.lines().take(30) {
                        let l = Line::from(vec![Span::styled(
                            format!("  ↳ {raw_line}"),
                            Style::default().fg(t.dim()),
                        )]);
                        lines.extend(wrap_line(l, wrap_w));
                    }
                    if r.lines().count() > 30 {
                        let l = Line::from(vec![Span::styled(
                            format!("  ↳ … {} more lines", r.lines().count() - 30),
                            Style::default().fg(t.dim()),
                        )]);
                        lines.extend(wrap_line(l, wrap_w));
                    }
                }
            }
        }
        Kind::Note => {
            // The engine's own words, labelled: glyph by NoteKind —
            // the same table the desktop renders (`chat.tcl
            // _note_glyph`), so both surfaces say it the same way.
            if let Some(n) = &m.note {
                // `NoteKind::glyph()`, not a copy of it. There were three
                // maps — this one, `notes::glyph()`, and the desktop's
                // `_note_glyph` — under a comment promising both surfaces
                // "say it the same way", and they had already drifted:
                // `Plugin` was ⚙ in the engine and • here. A 12th arm
                // would have perpetuated that; calling the source is
                // less code and cannot drift at all.
                let glyph = n.kind.glyph();
                let logical = Line::from(vec![
                    Span::styled(
                        format!("{glyph} "),
                        Style::default().fg(t.purple()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(n.summary.clone(), Style::default().fg(t.text())),
                ]);
                lines.extend(wrap_line(logical, wrap_w));
                if !n.detail.is_empty() {
                    for raw_line in n.detail.lines() {
                        let l = Line::from(vec![Span::styled(
                            format!("  {raw_line}"),
                            Style::default().fg(t.dim()),
                        )]);
                        lines.extend(wrap_line(l, wrap_w));
                    }
                }
                if let Some((key, value)) = &n.toggle {
                    let l = Line::from(vec![Span::styled(
                        format!("  ({key} = {value})"),
                        Style::default().fg(t.dim()),
                    )]);
                    lines.extend(wrap_line(l, wrap_w));
                }
            }
        }
        Kind::Status => {
            let logical = Line::from(vec![Span::styled(
                format!("· {}", m.body),
                Style::default().fg(t.dim()),
            )]);
            lines.extend(wrap_line(logical, wrap_w));
        }
    }
    MsgRender {
        kind: m.kind,
        len: src_len(m),
        collapsed: m.collapsed,
        lines,
        links,
    }
}

/// The staleness key for the render cache: how much source text a
/// block renders from. Body for prose, name+args+result for tools,
/// summary+detail for notes — anything that changes the render.
fn src_len(m: &veles_core::transcript::Block) -> usize {
    m.body.len()
        + m.tool
            .as_ref()
            .map(|t| t.args.len() + t.result.as_ref().map_or(0, |r| r.len()))
            .unwrap_or(0)
        + m.note
            .as_ref()
            .map(|n| n.summary.len() + n.detail.len())
            .unwrap_or(0)
}

fn draw_chat(f: &mut ratatui::Frame<'_>, area: Rect, st: &mut UiState) {
    let t = st.theme.clone();
    // The chat pane is [content | scrollbar]; the block border eats one
    // column on each side. Wrap width = inner width of the content area.
    let (chat_r, sb_r) = {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(area);
        (cols[0], cols[1])
    };
    let wrap_w = chat_r.width.saturating_sub(2) as usize;
    let inner_h = area.height.saturating_sub(2);

    // ── refresh only what changed ────────────────────────────────────
    if st.cache.width != wrap_w as u16 || st.cache.theme != t.name || st.cache.raw != st.raw_view {
        st.cache.invalidate_all();
        st.cache.width = wrap_w as u16;
        st.cache.theme = t.name.clone();
        st.cache.raw = st.raw_view;
    }
    {
        let (transcript, cache) = (&st.transcript, &mut st.cache);
        cache.msgs.resize(transcript.blocks.len(), None);
        for (i, m) in transcript.blocks.iter().enumerate() {
            let stale = match &cache.msgs[i] {
                Some(c) => c.kind != m.kind || c.len != src_len(m) || c.collapsed != m.collapsed,
                None => true,
            };
            if stale {
                cache.msgs[i] = Some(render_msg(m, &t, wrap_w, st.raw_view));
            }
        }
    }

    // Empty state: the synthwave scene, cheap and uncached.
    let splash: Vec<Line<'static>> = if st.transcript.is_empty() {
        let mut v = art_scene_spans(&t);
        v.push(Line::from(""));
        v.push(Line::from(Span::styled(
            "  type a message and press Enter to begin.",
            Style::default().fg(t.dim()),
        )));
        v.push(Line::from(Span::styled(
            "  /help for commands, /themes to pick a palette, /art to show the scene again.",
            Style::default().fg(t.dim()),
        )));
        v
    } else {
        Vec::new()
    };

    // Row offsets per message (search jumps) and the grand total.
    let mut msg_rows: Vec<usize> = Vec::with_capacity(st.transcript.len());
    let mut total_rows = splash.len();
    for slot in &st.cache.msgs {
        msg_rows.push(total_rows);
        total_rows += slot.as_ref().map(|c| c.lines.len()).unwrap_or(0);
    }
    let total = total_rows as u16;
    let max_off = total.saturating_sub(inner_h);

    st.msg_rows = msg_rows;
    if let Some(mi) = st.scroll_to_msg.take() {
        if let Some(row) = st.msg_rows.get(mi).copied() {
            let want = (row as u16).saturating_sub(2); // a little context above
            st.scrollback = max_off.saturating_sub(want.min(max_off));
        }
    }
    let scrollback = st.scrollback.min(max_off);
    let off = max_off - scrollback;

    // ── collect ONLY the visible rows ────────────────────────────────
    // This is the difference between scrolling being free and scrolling
    // re-cloning the entire transcript every wheel tick.
    let want = off as usize..(off as usize + inner_h as usize);
    let mut view: Vec<Line<'static>> = Vec::with_capacity(inner_h as usize);
    let mut cursor = 0usize;
    for l in &splash {
        if want.contains(&cursor) {
            view.push(l.clone());
        }
        cursor += 1;
    }
    st.link_hits.clear();
    st.all_links.clear();
    st.reasoning_header_rows.clear();
    for (tidx, slot) in st.cache.msgs.iter().enumerate() {
        let Some(c) = slot else { continue };
        let base = cursor;
        if base >= want.end {
            // Later messages are below the fold, but their links still
            // belong in the Ctrl-O list.
            for (_, _, _, url) in &c.links {
                st.all_links.push(url.clone());
            }
            cursor += c.lines.len();
            continue;
        }
        if base + c.lines.len() > want.start {
            for (i, l) in c.lines.iter().enumerate() {
                if want.contains(&(base + i)) {
                    view.push(l.clone());
                }
            }
            for (row, cs, ce, url) in &c.links {
                let abs = base + row;
                if want.contains(&abs) {
                    let screen_row = chat_r.y + 1 + (abs - want.start) as u16;
                    st.link_hits.push((
                        screen_row,
                        chat_r.x + 1 + *cs as u16,
                        chat_r.x + 1 + *ce as u16,
                        url.clone(),
                    ));
                }
            }
            // Track reasoning header rows for click-to-expand.
            if tidx < st.transcript.blocks.len()
                && st.transcript.blocks[tidx].kind == Kind::Reasoning
                && want.contains(&base)
            {
                let screen_row = chat_r.y + 1 + (base - want.start) as u16;
                st.reasoning_header_rows.push((screen_row, tidx));
            }
        }
        for (_, _, _, url) in &c.links {
            st.all_links.push(url.clone());
        }
        cursor += c.lines.len();
    }
    st.all_links.dedup();

    let title = st
        .sessions
        .iter()
        .find(|s| s.id == st.session)
        .map(|s| truncate(&s.label, 40))
        .unwrap_or_else(|| truncate(&st.session, 40));
    // No `.scroll()`: `view` already IS the visible window.
    let chat = Paragraph::new(view).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.hair()))
            .title(Span::styled(
                format!(" {title} "),
                Style::default().fg(t.pink()).add_modifier(Modifier::BOLD),
            ))
            .title_alignment(ratatui::layout::Alignment::Left),
    );
    f.render_widget(chat, chat_r);

    // Neon scrollbar (only when content overflows).
    let mut sb_state = ScrollbarState::new(total as usize).position(off as usize);
    let sb = Scrollbar::default()
        .orientation(ScrollbarOrientation::VerticalRight)
        .begin_symbol(Some("▲"))
        .end_symbol(Some("▼"))
        .track_symbol(Some("│"))
        .style(Style::default().fg(t.hair()))
        .thumb_style(Style::default().fg(t.pink()));
    f.render_stateful_widget(sb, sb_r, &mut sb_state);
}

/// Split a styled logical line into visual lines ≤ `width` columns,
/// char-by-char on display width (CJK counts 2). Wrapping is done HERE
/// so the scroll offset maps 1:1 to rows — ratatui's own Wrap would
/// silently make the visual height exceed the logical line count and
/// break bottom-following.
/// Strip ANSI escape sequences and neutralize other control bytes
/// before text reaches a ratatui widget. ratatui does NOT escape cell
/// content: an ESC/CR/BEL inside a tool result is written to the tty
/// verbatim and the terminal EMULATOR interprets it — one
/// `opencode serve` log wall full of color codes and progress \r's
/// and the whole screen is garbage, which is exactly the "terminal
/// dumped a load of shit into the tui" report. Tool output is
/// untrusted-shaped by definition, so this is the render-boundary
/// fence. Escape sequences vanish entirely (formatting noise); every
/// other C0 control becomes a visible '·' — junk stays SEEABLE, it
/// does not silently vanish. \n never reaches a widget (rows split on
/// it first) and \t survives as structure.
fn scrub_controls(s: &str) -> String {
    enum St {
        Normal,
        /// Saw a bare ESC; the NEXT char decides (CSI '[', OSC ']',
        /// or any two-char escape — ESC M, ESC 7 …).
        Esc,
        /// Inside CSI: params/intermediates until a final byte 0x40..=0x7E.
        Csi,
        /// Inside OSC: until BEL or ST (ESC \).
        Osc,
        /// Inside OSC, saw ESC — '\\' ends it, anything else restarts.
        OscEsc,
    }
    let mut out = String::with_capacity(s.len());
    let mut st = St::Normal;
    for c in s.chars() {
        match st {
            St::Normal => match c {
                '\x1b' => st = St::Esc,
                '\t' | '\n' => out.push(c),
                c if c.is_control() => out.push('·'),
                c => out.push(c),
            },
            St::Esc => match c {
                '[' => st = St::Csi,
                ']' => st = St::Osc,
                // Two-char escape: both bytes dropped.
                _ => st = St::Normal,
            },
            St::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    st = St::Normal;
                }
            }
            St::Osc => {
                if c == '\x07' {
                    st = St::Normal;
                } else if c == '\x1b' {
                    st = St::OscEsc;
                }
            }
            St::OscEsc => st = if c == '\\' { St::Normal } else { St::Osc },
        }
    }
    out
}

fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    wrap_line_tracked(line, width)
        .into_iter()
        .map(|(l, _)| l)
        .collect()
}

/// Same wrap, but each visual row also reports the range of SOURCE
/// columns it covers. That range is what maps a link's position in the
/// logical line onto a clickable region on screen — link identity can't
/// ride inside a `Span` (ratatui's `Style` has nowhere to put it) and
/// must not ride inside the text either (OSC 8 escapes would be counted
/// as characters and split apart by the cell-wise renderer).
fn wrap_line_tracked(
    line: Line<'static>,
    width: usize,
) -> Vec<(Line<'static>, std::ops::Range<usize>)> {
    use unicode_width::UnicodeWidthChar;
    let mut out: Vec<(Line<'static>, std::ops::Range<usize>)> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    let mut src_col = 0usize; // column in the unwrapped logical line
    let mut row_start = 0usize;
    // Characters are accumulated into RUNS, not one Span each: a span
    // per character meant ~1 String allocation per visible character per
    // frame, which is most of why a long transcript pinned a core.
    for span in line.spans {
        let style = span.style;
        // The render-boundary fence: nothing control-shaped reaches a
        // cell (see scrub_controls). Sanitized HERE so the width math
        // below counts what will actually be on screen.
        let content = scrub_controls(&span.content);
        let mut buf = String::new();
        for c in content.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(1);
            if cur_w + w > width && (!cur.is_empty() || !buf.is_empty()) {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), style));
                }
                out.push((Line::from(std::mem::take(&mut cur)), row_start..src_col));
                cur_w = 0;
                row_start = src_col;
            }
            buf.push(c);
            cur_w += w;
            src_col += w;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, style));
        }
    }
    if !cur.is_empty() {
        out.push((Line::from(std::mem::take(&mut cur)), row_start..src_col));
    }
    if out.is_empty() {
        out.push((Line::from(""), 0..0));
    }
    out
}

fn draw_input(f: &mut ratatui::Frame<'_>, area: Rect, st: &mut UiState) {
    let t = &st.theme;
    st.input_area = Some(area);
    let title = if st.busy {
        format!(" {} {} ", SPINNER[st.spinner], "thinking…")
    } else {
        " input ".to_string()
    };
    let title_style = if st.busy {
        Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(t.purple()).add_modifier(Modifier::BOLD)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(t.hair()))
        .title(Span::styled(title, title_style));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 3 || inner.height == 0 {
        return;
    }
    // A row per LINE — no word wrap: long lines scroll horizontally
    // with the caret instead (editor, not chat balloon). The caret's
    // row is the only one that scrolls; every other row clips at the
    // right edge, which is where the eye isn't.
    let lines: Vec<&str> = st.input.split('\n').collect();
    let (crow, ccol) = caret_line_col(&st.input, st.caret);
    st.input_vscroll = ensure_caret_row_visible(st.input_vscroll, crow, inner.height);
    let text_w = (inner.width as usize).saturating_sub(2); // past "❯ "
    let off = hscroll_offset(
        UnicodeWidthStr::width(&lines[crow.min(lines.len() - 1)][..ccol]),
        text_w,
    );
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (i, l) in lines
        .iter()
        .enumerate()
        .skip(st.input_vscroll as usize)
        .take(inner.height as usize)
    {
        let prefix = if i == 0 {
            Span::styled(
                "❯ ",
                Style::default().fg(t.pink()).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let content = if i == crow {
            skip_width(l, off).to_string()
        } else {
            scrub_controls(l).to_string()
        };
        rows.push(Line::from(vec![
            prefix,
            Span::styled(content, Style::default().fg(t.text())),
        ]));
    }
    f.render_widget(Paragraph::new(rows), inner);
}

/// The plan a route draws from, and what it has used — from the
/// tokenplan cache, which is an in-process read with NO I/O (the
/// gateway's background warm is what keeps it current).
///
/// `None` when the tokenplan is off, when nothing has been fetched yet,
/// or when the provider is on no plan this account reads. All three
/// render as no field at all, which is the honest answer: a figure the
/// engine has not got must never be invented, and a pool this session
/// cannot draw down must never be shown as if it were its own.
fn plan_reading_for(
    cfg: &veles_core::config::VelesConfig,
    provider: &str,
) -> Option<(String, f64)> {
    if !cfg.tokenplan.enabled {
        return None;
    }
    let plan = veles_core::tokenplan::plan_for_provider(provider)?;
    let (snap, _age) = veles_core::tokenplan::for_display(cfg)?;
    let frac = plan.tightest_used_fraction(&snap)?;
    Some((plan.label().to_string(), frac))
}

fn draw_status(f: &mut ratatui::Frame<'_>, area: Rect, st: &UiState) {
    let t = &st.theme;
    f.render_widget(Block::default().style(Style::default().bg(t.bg())), area);
    let mut left: Vec<Span> = Vec::new();
    // PLAN MODE FIRST, and in gold rather than dim. A client that is
    // refusing every edit needs to say so where the eye already goes;
    // buried at the far end it reads as decoration, and the operator
    // spends a turn wondering why nothing happened.
    if st.plan_mode {
        left.push(Span::styled(
            "▲ PLAN ",
            Style::default().fg(t.bg()).bg(t.gold()),
        ));
        left.push(Span::raw(" "));
    }
    if st.busy {
        left.push(Span::styled(
            format!("{} ", SPINNER[st.spinner]),
            Style::default().fg(t.cyan()),
        ));
        left.push(Span::styled(
            if st.tool_running.is_some() {
                format!("⚙ {}…", st.tool_running.as_deref().unwrap_or(""))
            } else {
                "thinking…".to_string()
            },
            Style::default().fg(t.cyan()),
        ));
    } else {
        left.push(Span::styled("● ready", Style::default().fg(t.green())));
    }
    if !st.route.is_empty() {
        left.push(Span::styled(
            format!(" · {}", st.route),
            Style::default().fg(t.dim()),
        ));
    }
    if !st.model.is_empty() {
        left.push(Span::styled(
            format!(" · {}", truncate(&st.model, 24)),
            Style::default().fg(t.dim()),
        ));
    }
    // THE UPSTREAM PLAN, with its name (USER 2026-09-20). Four providers
    // on this box draw on four disjoint pools; a bare percentage would
    // be the conflation being complained of, so the pool's own label
    // rides with the figure. Gold past three quarters, red past ninety —
    // the same thresholds every other surface uses, because a field that
    // means something else on each frontend is worse than no field.
    if let Some((label, frac)) = &st.plan_reading {
        let pct = (frac * 100.0).round() as i64;
        let colour = if pct >= 90 {
            t.red()
        } else if pct >= 75 {
            t.gold()
        } else {
            t.dim()
        };
        left.push(Span::styled(
            format!(" · {label} {pct}%"),
            Style::default().fg(colour),
        ));
    }
    if st.prompt_tok + st.completion_tok > 0 {
        // The uncached figure rides only once some turn reported a cache
        // field — printing 0 before then reads as "nothing cached".
        let uncached = if st.cache_seen {
            format!(
                " · {} uncached",
                st.prompt_tok.saturating_sub(st.cached_tok)
            )
        } else {
            String::new()
        };
        // Reasoning, when a route splits it out: completion is really
        // answer + thinking, so name both — the point of the separate
        // count is making an overthinking model legible instead of just
        // expensive-looking. None renders nothing rather than a "0".
        let split = if let Some(rt) = st.reasoning_tok {
            let answer = st.completion_tok.saturating_sub(rt);
            format!(" · {answer} answer · {rt} thinking")
        } else {
            String::new()
        };
        left.push(Span::styled(
            format!(
                " · {}→{} tok{uncached}{split}",
                st.prompt_tok, st.completion_tok
            ),
            Style::default().fg(t.dim()),
        ));
    }
    left.push(Span::styled(
        format!(" · {}", truncate(&st.session, 18)),
        Style::default().fg(t.purple()),
    ));

    let mode_label = match st.keymap {
        veles_core::keymap::Keymap::Normie => "· normie",
        veles_core::keymap::Keymap::Emacs => "· emacs",
        veles_core::keymap::Keymap::Vi => {
            if st.vi_insert {
                "· vi (INSERT)"
            } else {
                "· vi (NORMAL)"
            }
        }
    };
    let right = Span::styled(
        format!("{mode_label} · F1 help · F2 log · F3 source · Ctrl-O links"),
        Style::default().fg(t.dim()),
    );
    let left_w = left.iter().map(|s| s.content.width()).sum::<usize>() as u16;
    let right_w = right.content.width() as u16;
    let pad = area.width.saturating_sub(left_w + right_w + 1) as usize;
    let mut full = left;
    full.push(Span::raw(" ".repeat(pad)));
    full.push(right);
    f.render_widget(Paragraph::new(Line::from(full)), area);
}

/// Palette entries matching the current query (exact substring, then
/// a looser head match as a fallback).
///
/// The description is searchable too: "what runs a shell command" should
/// find `/background` and `/shells` even though neither label spells any
/// of those words. Labels stay the primary key — a hit there outranks a
/// hit in the prose — so the fallback pass is the description's home.
fn filtered_palette(st: &UiState) -> Vec<PaletteEntry> {
    let q = st
        .palette
        .as_ref()
        .map(|p| p.query.to_lowercase())
        .unwrap_or_default();
    let all = palette_entries(st);
    let by_label: Vec<PaletteEntry> = all
        .iter()
        .filter(|e| q.is_empty() || e.label.to_lowercase().contains(&q))
        .cloned()
        .collect();
    if !by_label.is_empty() {
        return by_label;
    }
    // Nothing in a label. Try the head (so `todos` finds `/todos`), then
    // the description.
    let mut v: Vec<PaletteEntry> = all
        .iter()
        .filter(|e| {
            e.label
                .split(':')
                .next()
                .map(|h| h.to_lowercase().contains(&q))
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    if v.is_empty() {
        v = all
            .into_iter()
            .filter(|e| !q.is_empty() && e.desc.to_lowercase().contains(&q))
            .collect();
    }
    v
}

fn draw_palette(f: &mut ratatui::Frame<'_>, st: &UiState, t: &veles_core::theme::Theme) {
    let area = centered_rect(72, 60, f.size());
    let entries = filtered_palette(st);
    let mut lines: Vec<Line> = Vec::new();
    let q = st
        .palette
        .as_ref()
        .map(|p| p.query.clone())
        .unwrap_or_default();
    lines.push(Line::from(vec![
        Span::styled(
            "M-x ",
            Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(q.clone(), Style::default().fg(t.text())),
    ]));
    lines.push(Line::from(""));
    let sel = st.palette.as_ref().map(|p| p.sel).unwrap_or(0);
    // The description column starts after the widest label ON SCREEN,
    // so rows line up into a readable block instead of a ragged right
    // edge. Capped, because one long session name would otherwise push
    // every description off the popup.
    let label_w = entries
        .iter()
        .take(12)
        .map(|e| e.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(area.width.saturating_sub(30) as usize);
    for (i, entry) in entries.iter().enumerate().take(12) {
        let marker = if i == sel { "\u{25b8} " } else { "  " };
        let style = if i == sel {
            Style::default()
                .fg(t.bg())
                .bg(t.pink())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.text())
        };
        let mut row = format!("{marker}{}", entry.label);
        if !entry.desc.is_empty() {
            // `label_w` is in CHARACTERS while `format!` pads by bytes —
            // a multibyte label would be over-padded and shove the
            // description out of line, so pad by hand.
            let pad = label_w.saturating_sub(entry.label.chars().count()) + 2;
            row.push_str(&" ".repeat(pad));
            row.push_str(&truncate(&entry.desc, 40));
        }
        lines.push(Line::from(Span::styled(row, style)));
    }
    if entries.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matching commands)",
            Style::default().fg(t.dim()),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  \u{2191}\u{2193} select \u{b7} Enter run \u{b7} Esc close",
        Style::default().fg(t.dim()),
    )));
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.purple()))
                .title(Span::styled(
                    " command palette ",
                    Style::default().fg(t.purple()).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(t.panel())),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(popup, area);
}

fn draw_picker(f: &mut ratatui::Frame<'_>, st: &UiState, t: &veles_core::theme::Theme) {
    let area = centered_rect(66, 60, f.size());
    let picker = match &st.picker {
        Some(p) => p,
        None => return,
    };
    let width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        picker.title.clone(),
        Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    let mut shown = 0usize;
    for (i, row) in picker.rows.iter().enumerate() {
        let marker = if i == picker.sel { "\u{25b8} " } else { "  " };
        let style = if i == picker.sel {
            Style::default()
                .fg(t.bg())
                .bg(t.pink())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.text())
        };
        let clipped: String = row.label.chars().take(width).collect();
        lines.push(Line::from(Span::styled(
            format!("{marker}{clipped}"),
            style,
        )));
        shown += 1;
        if shown >= 12 {
            break;
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  \u{2191}\u{2193} select \u{b7} Enter run \u{b7} Esc close",
        Style::default().fg(t.dim()),
    )));
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.purple()))
                .title(Span::styled(
                    format!(" {} ", picker.title),
                    Style::default().fg(t.purple()).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(t.panel())),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(popup, area);
}

/// The detail pager: a scrollable, read-only block of text opened by a
/// `View` picker row. Long lines clip rather than wrap so the scroll
/// offset is a simple line count.
fn draw_detail(f: &mut ratatui::Frame<'_>, st: &UiState, t: &veles_core::theme::Theme) {
    let area = centered_rect(80, 82, f.size());
    let d = match &st.detail {
        Some(d) => d,
        None => return,
    };
    let total = d.text.lines().count();
    let inner_h = area.height.saturating_sub(3) as usize; // border + fetch line
    let max_scroll = total.saturating_sub(inner_h);
    let off = d.scroll.min(max_scroll);
    let lines: Vec<Line<'static>> = d
        .text
        .lines()
        .skip(off)
        .take(inner_h)
        .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(t.text()))))
        .collect();
    let footer = format!(
        " {} / {} lines · ↑↓ PgUp/PgDn scroll · Esc/q close ",
        off + 1,
        total
    );
    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.cyan()))
            .title(Span::styled(
                format!(
                    " {} ",
                    truncate(&d.title, area.width.saturating_sub(8) as usize)
                ),
                Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(footer, Style::default().fg(t.dim())))
            .style(Style::default().bg(t.panel())),
    );
    f.render_widget(Clear, area);
    f.render_widget(p, area);
}

fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(v[1])[1]
}

// ── the ask popup's keyboard model (2026-09-26 parity audit) ────────
//
// Extracted from the event loop as pure functions so the multi-select
// and multi-question flows are testable without a terminal: `ask_rows`
// builds the flat selectable model, `ask_key` maps a key to an effect,
// `commit_ask` assembles the answer. The loop only applies effects to
// state and resolves.

/// One selectable row of the ask popup. `addr` ("q2:c1") identifies
/// the choice across questions; the Other row types free text for its
/// question (the affordance the desktop's amend entry always had).
#[derive(Debug, Clone, PartialEq)]
enum AskRow {
    /// A question's prompt inside a multi-question ask (not
    /// selectable — a visual grouping header).
    Header {
        text: String,
    },
    Choice {
        addr: String,
        label: String,
    },
    Other {
        qk: String,
    },
}

/// The flat row list: single-question asks lay out choices + one
/// Other row; multi-question asks repeat that per question with a
/// header. A no-choices ask has NO rows (typing goes to the plain
/// input, the legacy path). Only a clarify gets text rows: an
/// approval's or failover's answer is one of its choices, never prose.
/// Does this ask take a NOTE beside its choice keys? Approvals and
/// failovers answer on single keys (see `ask_key`), so their note lives on
/// a line of its own that Tab gives the keys to. Text-shaped asks
/// (clarify, ticket, challenge) already type into their rows.
fn takes_note(req: &veles_core::ask::AskRequest) -> bool {
    !matches!(
        req.kind,
        veles_core::ask::AskKind::Clarify
            | veles_core::ask::AskKind::Ticket
            | veles_core::ask::AskKind::Challenge
    )
}

/// Attach the typed note to a key's answer, for asks that take one. An
/// answer that already carries text (an `amend:` typed as the key) keeps
/// its own.
fn with_note(
    req: &veles_core::ask::AskRequest,
    mut a: veles_core::ask::AskAnswer,
    note: &str,
) -> veles_core::ask::AskAnswer {
    let note = note.trim();
    if takes_note(req) && !note.is_empty() && a.free_text.is_none() && !a.timed_out {
        a.free_text = Some(note.to_string());
    }
    a
}

fn ask_rows(req: &veles_core::ask::AskRequest) -> Vec<AskRow> {
    let multi_q = req.questions.len() > 1;
    // A ticket ask takes text too: the ticket, pasted (D3).
    let text_rows = matches!(
        req.kind,
        veles_core::ask::AskKind::Clarify
            | veles_core::ask::AskKind::Ticket
            | veles_core::ask::AskKind::Challenge
    );
    let mut rows = Vec::new();
    for q in req.all_questions() {
        if req.questions.len() > 1 {
            rows.push(AskRow::Header {
                text: q.prompt.clone(),
            });
        }
        for c in &q.choices {
            rows.push(AskRow::Choice {
                addr: format!("{}:{}", q.key, c.key),
                label: c.label.clone(),
            });
        }
        // EVERY question of a multi-question ask carries a text row —
        // for a choices question it is the "Other" affordance, for a
        // free-text question it is THE answer field (mirroring the Tk
        // dialog). One text blob spread over several questions is
        // guessing, and guessing is what `parse_reply` now refuses.
        if text_rows && (!q.choices.is_empty() || multi_q) {
            rows.push(AskRow::Other { qk: q.key.clone() });
        }
    }
    rows
}

/// Where typed characters land for the selected row: an Other row's
/// buffer, the plain input (no-choices ask), or nowhere (a choice
/// row — letters there are answer attempts, not prose).
#[derive(Debug, PartialEq)]
enum TypingTarget {
    Other(String),
    Plain,
    Nowhere,
}

fn ask_typing_target(req: &veles_core::ask::AskRequest, sel: usize) -> TypingTarget {
    let rows = ask_rows(req);
    match rows.get(sel) {
        Some(AskRow::Other { qk }) => TypingTarget::Other(qk.clone()),
        Some(AskRow::Choice { .. }) | Some(AskRow::Header { .. }) => TypingTarget::Nowhere,
        None if rows.is_empty() => TypingTarget::Plain,
        None => TypingTarget::Nowhere,
    }
}

/// What a key does to the ask popup. `Answer` resolves immediately
/// (legacy single-tap paths); the rest are state edits the loop owns.
#[derive(Debug)]
enum AskKeyEffect {
    Answer(veles_core::ask::AskAnswer),
    SetSel(usize),
    /// Toggle a choice. `radio` = the question is single-select, so
    /// toggling clears the question's other selections first.
    Toggle {
        addr: String,
        radio: bool,
    },
    Type(char),
    Backspace,
    /// Enter: assemble the answer from toggles/other text/input.
    Commit,
    ScrollUp,
    ScrollDown,
    None,
}

fn ask_key(
    req: &veles_core::ask::AskRequest,
    sel: usize,
    _toggles: &std::collections::BTreeSet<String>,
    key: &KeyCode,
) -> AskKeyEffect {
    use veles_core::ask::AskAnswer;
    // An approval or a failover keeps the key model it always had:
    // Space and PgDn scroll (a plan-mode approval carries the whole
    // plan), Esc denies, and only an explicit choice key — 1-4, y, n —
    // answers. No cursor, so nothing is "highlighted" for Enter or
    // Space to pick: the parity round's cursor made Space (the scroll
    // key) and Enter (a composer Enter racing the popup) approve row 0,
    // which is "allow once". A ticket ask is typed into like a clarify:
    // its answer is the pasted ticket.
    if !matches!(
        req.kind,
        veles_core::ask::AskKind::Clarify
            | veles_core::ask::AskKind::Ticket
            | veles_core::ask::AskKind::Challenge
    ) {
        return match key {
            KeyCode::PageUp => AskKeyEffect::ScrollUp,
            KeyCode::PageDown | KeyCode::Char(' ') => AskKeyEffect::ScrollDown,
            KeyCode::Esc => AskKeyEffect::Answer(AskAnswer::choice(veles_core::ask::CHOICE_DENY)),
            KeyCode::Char(c) => veles_core::ask::parse_reply(req, &c.to_string())
                .map(AskKeyEffect::Answer)
                .unwrap_or(AskKeyEffect::None),
            _ => AskKeyEffect::None,
        };
    }
    // In a text answer every character is text — `j`, `k`, digits and
    // Space included. Only the arrows leave the row.
    if let KeyCode::Char(c) = key {
        if matches!(
            ask_typing_target(req, sel),
            TypingTarget::Other(_) | TypingTarget::Plain
        ) {
            return AskKeyEffect::Type(*c);
        }
    }
    let rows = ask_rows(req);
    // The legacy single-tap regime: one question, single-select. Every
    // choice key answers instantly; letters match labels. Multi-select
    // and multi-question asks switch to toggle+commit instead.
    let instant = req.questions.len() <= 1 && !req.multi && !req.choices.is_empty();
    let n = rows.len();
    match key {
        KeyCode::PageUp => AskKeyEffect::ScrollUp,
        KeyCode::PageDown => AskKeyEffect::ScrollDown,
        KeyCode::Esc => AskKeyEffect::Answer(AskAnswer::choice(veles_core::ask::CHOICE_DENY)),
        KeyCode::Up | KeyCode::Char('k') => AskKeyEffect::SetSel(sel.saturating_sub(1)),
        KeyCode::Down | KeyCode::Char('j') => {
            AskKeyEffect::SetSel((sel + 1).min(n.saturating_sub(1)))
        }
        KeyCode::Enter => AskKeyEffect::Commit,
        KeyCode::Backspace => match ask_typing_target(req, sel) {
            TypingTarget::Other(_) | TypingTarget::Plain => AskKeyEffect::Backspace,
            TypingTarget::Nowhere => AskKeyEffect::None,
        },
        KeyCode::Char(' ') => match rows.get(sel) {
            Some(AskRow::Choice { addr, .. }) => {
                if instant {
                    let key = addr.split(':').nth(1).unwrap_or("").to_string();
                    AskKeyEffect::Answer(AskAnswer::choice(&key))
                } else {
                    let qk = addr.split(':').next().unwrap_or("").to_string();
                    let multi = req
                        .all_questions()
                        .iter()
                        .find(|q| q.key == qk)
                        .map(|q| q.multi)
                        .unwrap_or(false);
                    AskKeyEffect::Toggle {
                        addr: addr.clone(),
                        radio: !multi,
                    }
                }
            }
            _ => AskKeyEffect::None,
        },
        KeyCode::Char(c) => {
            // Digits jump to (and in the instant regime, answer) the
            // n-th choice row across the whole list.
            if let Some(d) = c.to_digit(10) {
                let d = d as usize;
                if d >= 1 {
                    let mut choice_no = 0usize;
                    for (i, r) in rows.iter().enumerate() {
                        if let AskRow::Choice { addr, .. } = r {
                            choice_no += 1;
                            if choice_no == d {
                                if instant {
                                    let key = addr.split(':').nth(1).unwrap_or("").to_string();
                                    return AskKeyEffect::Answer(AskAnswer::choice(&key));
                                }
                                return AskKeyEffect::SetSel(i);
                            }
                        }
                    }
                }
                return AskKeyEffect::None;
            }
            // A choice row (text rows returned above): a letter/label
            // prefix answers in the instant regime.
            if instant {
                if let Some(a) = veles_core::ask::parse_reply(req, &c.to_string()) {
                    return AskKeyEffect::Answer(a);
                }
            }
            AskKeyEffect::None
        }
        _ => AskKeyEffect::None,
    }
}

/// Enter: assemble the whole answer from the current state. Returns
/// None when there is nothing to send (Enter is a no-op rather than
/// an empty answer).
fn commit_ask(
    req: &veles_core::ask::AskRequest,
    sel: usize,
    toggles: &std::collections::BTreeSet<String>,
    other: &std::collections::BTreeMap<String, String>,
    input: &str,
) -> Option<veles_core::ask::AskAnswer> {
    use veles_core::ask::AskAnswer;
    // Nothing assembles an approval or failover answer: those answer
    // on an explicit key only (see ask_key).
    if !matches!(
        req.kind,
        veles_core::ask::AskKind::Clarify
            | veles_core::ask::AskKind::Ticket
            | veles_core::ask::AskKind::Challenge
    ) {
        return None;
    }
    // The no-choices ask keeps its exact legacy shape.
    if req.questions.len() <= 1 && req.choices.is_empty() {
        return Some(AskAnswer::text(input.trim()));
    }
    let questions = req.all_questions();
    let mut per: std::collections::BTreeMap<String, AskAnswer> = std::collections::BTreeMap::new();
    for q in &questions {
        let prefix = format!("{}:", q.key);
        let mut selected: Vec<String> = toggles
            .iter()
            .filter(|a| a.starts_with(&prefix))
            .filter_map(|a| a.split(':').nth(1).map(str::to_string))
            .filter(|k| k != "other")
            .collect();
        selected.sort();
        let otext = other
            .get(&q.key)
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if !selected.is_empty() || !otext.is_empty() {
            let mut a = AskAnswer {
                selected,
                ..Default::default()
            };
            if !otext.is_empty() {
                a.free_text = Some(otext);
            }
            per.insert(q.key.clone(), a);
        }
    }
    if !per.is_empty() {
        return Some(if req.questions.len() > 1 {
            AskAnswer::from_per_question(per)
        } else {
            per.into_values().next().unwrap()
        });
    }
    // Nothing toggled, nothing typed: the highlighted choice row is
    // the answer (Enter on a fresh dialog picks the cursor's row —
    // the keyboard equivalent of the focused default button).
    match ask_rows(req).into_iter().nth(sel) {
        Some(AskRow::Choice { addr, .. }) => {
            let mut it = addr.split(':');
            let qk = it.next().unwrap_or("q1").to_string();
            let ck = it.next().unwrap_or("").to_string();
            if req.questions.len() > 1 {
                let mut per: std::collections::BTreeMap<String, AskAnswer> =
                    std::collections::BTreeMap::new();
                per.insert(qk, AskAnswer::choice(&ck));
                Some(AskAnswer::from_per_question(per))
            } else {
                Some(AskAnswer::choice(&ck))
            }
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_ask(
    f: &mut ratatui::Frame<'_>,
    req: &veles_core::ask::AskRequest,
    ask_input: &str,
    note_mode: bool,
    ask_scroll: u16,
    sel: usize,
    toggles: &std::collections::BTreeSet<String>,
    other: &std::collections::BTreeMap<String, String>,
    t: &veles_core::theme::Theme,
) {
    // 40% of the screen truncated a long plan's choices off the
    // bottom (the same plan-mode defect the desktop round fixed): a
    // big DETAIL needs a big popup, and the popup scrolls (PgUp/PgDn
    // while it is up — see the ask-mode key arm). Multi-question asks
    // need the same headroom: 4 questions × (header + 4 choices +
    // Other) overflows 40% of a laptop screen.
    let detail_lines = req.detail.as_ref().map(|d| d.lines().count()).unwrap_or(0);
    let question_rows: usize = req
        .all_questions()
        .iter()
        .map(|q| q.choices.len() + 2)
        .sum::<usize>()
        + req.questions.len().max(1);
    let pct_y = if detail_lines > 30 || question_rows > 12 {
        80
    } else {
        40
    };
    let area = centered_rect(70, pct_y, f.size());
    let (accent, title) = match req.kind {
        veles_core::ask::AskKind::Approval => (t.pink(), " ⚠ approval "),
        veles_core::ask::AskKind::Clarify => (t.cyan(), " ? question "),
        veles_core::ask::AskKind::Failover => (t.gold(), " ⇄ failover "),
        veles_core::ask::AskKind::Secret => (t.pink(), " ◆ secret needed "),
        veles_core::ask::AskKind::Ticket => (t.gold(), " ✎ ticket needed "),
        veles_core::ask::AskKind::Challenge => (t.pink(), " ⛔ challenge "),
    };
    let questions = req.all_questions();
    let multi_q = req.questions.len() > 1;
    let is_clarify = req.kind == veles_core::ask::AskKind::Clarify;
    // A multi-question ask does not repeat the first question as a
    // title — every question is a numbered section below.
    let mut lines: Vec<Line> = if multi_q {
        Vec::new()
    } else {
        vec![Line::from(Span::styled(
            req.prompt.clone(),
            Style::default().fg(t.text()).add_modifier(Modifier::BOLD),
        ))]
    };
    if let Some(d) = &req.detail {
        for l in d.lines() {
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Style::default().fg(t.purple()),
            )));
        }
    }
    // The TUI has no masked field: the value goes in through the CLI,
    // which reads it with echo off, and this popup only says "set".
    if req.kind == veles_core::ask::AskKind::Secret {
        lines.push(Line::from(Span::styled(
            format!(
                "  Store it privately: `veles secret set {}` in another terminal \
                 (it does not echo), then press 1. Never type the value here.",
                req.prompt
            ),
            Style::default().fg(t.gold()),
        )));
    }
    // A ticket is signed with `veles ticket sign` (in another terminal
    // here, or on another device) and pasted into the text row.
    if req.kind == veles_core::ask::AskKind::Ticket {
        lines.push(Line::from(Span::styled(
            "  Sign it: `veles ticket sign '<the request above>'`, \
             then paste the ticket into the text row below.",
            Style::default().fg(t.gold()),
        )));
    }
    // A terminal cannot show the screenshot: the desktop can, and so can
    // a real window when the node running the browser has a display.
    if req.kind == veles_core::ask::AskKind::Challenge {
        lines.push(Line::from(Span::styled(
            "  The page is on the desktop as a picture, or pick `open a window`. \
             Here: `click X Y`, `type TEXT` or `key Enter` in the text row.",
            Style::default().fg(t.pink()),
        )));
    }
    lines.push(Line::from(""));
    // The flat cursor model mirrors `ask_rows` — keep the two in lock
    // step or the highlight lands on the wrong row.
    let mut row_idx = 0usize;
    for (qi, q) in questions.iter().enumerate() {
        if multi_q {
            if qi > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                format!("{}. {}", qi + 1, q.prompt),
                Style::default().fg(t.text()).add_modifier(Modifier::BOLD),
            )));
            if q.multi && !q.choices.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  (pick any — Space toggles, several allowed)",
                    Style::default().fg(t.dim()),
                )));
            }
            // A header row occupies a slot in `ask_rows`' flat model;
            // the cursor must count it or it drifts off by one per
            // question.
            row_idx += 1;
        }
        for (i, c) in q.choices.iter().enumerate() {
            let toggled = toggles.contains(&format!("{}:{}", q.key, c.key));
            // No cursor on an approval/failover: a highlighted row reads
            // as the default, and those answer on an explicit key only.
            let selected_here = is_clarify && row_idx == sel;
            let marker = if multi_q || q.multi {
                if toggled {
                    "◉"
                } else {
                    "○"
                }
            } else {
                "•"
            };
            let num = Span::styled(
                format!("  {} {}) ", if selected_here { "❯" } else { " " }, i + 1),
                Style::default()
                    .fg(if selected_here || toggled {
                        t.gold()
                    } else {
                        t.dim()
                    })
                    .add_modifier(Modifier::BOLD),
            );
            let mark = Span::styled(
                format!("{marker} "),
                Style::default().fg(if toggled { t.cyan() } else { t.dim() }),
            );
            let label = Span::styled(
                c.label.clone(),
                Style::default()
                    .fg(t.text())
                    .add_modifier(if selected_here {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            );
            lines.push(Line::from(vec![num, mark, label]));
            row_idx += 1;
        }
        // The text row: "Other:" next to choices, the answer field for
        // a free-text question (mirroring ask_rows).
        if !q.choices.is_empty() || multi_q {
            let selected_here = row_idx == sel;
            let otext = other.get(&q.key).cloned().unwrap_or_default();
            let otext_empty = otext.is_empty();
            let num = Span::styled(
                format!("  {} ", if selected_here { "❯" } else { " " }),
                Style::default()
                    .fg(if selected_here { t.gold() } else { t.dim() })
                    .add_modifier(Modifier::BOLD),
            );
            lines.push(Line::from(vec![
                num,
                Span::styled(
                    if multi_q || q.multi { "○ " } else { "  " },
                    Style::default().fg(t.dim()),
                ),
                Span::styled(
                    if !q.choices.is_empty() { "Other: " } else { "" },
                    Style::default().fg(t.cyan()),
                ),
                Span::styled(otext, Style::default().fg(t.text())),
                if otext_empty {
                    Span::styled("(type here)", Style::default().fg(t.dim()))
                } else {
                    Span::raw("")
                },
            ]));
            row_idx += 1;
        }
    }
    if questions.iter().all(|q| q.choices.is_empty()) {
        lines.push(Line::from(Span::styled(
            "  (type an answer, Enter to send)",
            Style::default().fg(t.dim()),
        )));
        lines.push(Line::from(vec![
            Span::styled("  > ", Style::default().fg(t.cyan())),
            Span::styled(ask_input.to_string(), Style::default().fg(t.text())),
        ]));
    }
    if takes_note(req) && (note_mode || !ask_input.is_empty()) {
        lines.push(Line::from(vec![
            Span::styled(
                if note_mode {
                    "  ❯ note: "
                } else {
                    "    note: "
                },
                Style::default().fg(if note_mode { t.gold() } else { t.cyan() }),
            ),
            Span::styled(ask_input.to_string(), Style::default().fg(t.text())),
            if note_mode {
                Span::styled("▏", Style::default().fg(t.gold()))
            } else {
                Span::raw("")
            },
        ]));
    }
    lines.push(Line::from(""));
    let hint = if takes_note(req) && note_mode {
        "  typing a note · Enter sends the note alone · Tab/Esc = back to the keys"
    } else if takes_note(req) {
        "  1-4 picks (with the note) · Tab = write a note · Esc = deny/skip"
    } else if !is_clarify {
        "  1-4 picks · Esc = deny/skip"
    } else if multi_q || req.multi {
        "  ↑/↓ move · Space toggles · Other rows take text · Enter sends · Esc = deny/skip"
    } else if !req.choices.is_empty() {
        "  1-4 or ↑/↓+Space picks · Other row types free text · Enter sends · Esc = deny/skip"
    } else {
        "  Esc = deny/skip"
    };
    lines.push(Line::from(Span::styled(
        format!(
            "{}{}",
            hint,
            if detail_lines > 30 {
                " · PgUp/PgDn = scroll the plan"
            } else {
                ""
            }
        ),
        Style::default().fg(t.dim()),
    )));
    let popup = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .title(Span::styled(
                    title,
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(t.panel())),
        )
        .scroll((ask_scroll, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, area);
    f.render_widget(popup, area);
}

fn draw_help(f: &mut ratatui::Frame<'_>, t: &veles_core::theme::Theme) {
    let area = centered_rect(60, 70, f.size());
    let rows = [
        ("Enter", "send · run selected session"),
        ("Esc", "cancel turn / quit"),
        ("Ctrl-C", "cancel turn / quit"),
        ("Ctrl/Alt/⇧-Enter", "newline (multiline composer)"),
        ("↑/↓", "move between lines · history at the edges"),
        ("←/→ Home End", "move caret"),
        ("Ctrl-U", "clear input"),
        ("Ctrl-W", "kill word"),
        ("Ctrl-K / Alt-x", "command palette (type to filter)"),
        ("Ctrl-/", "undo the last composer edit"),
        ("Ctrl-Z", "suspend to the shell (fg to return)"),
        ("Tab / ⇧Tab", "switch session"),
        ("Ctrl-N", "new session"),
        ("Ctrl-D", "delete session (empty input)"),
        ("PgUp / PgDn", "scroll transcript"),
        ("Ctrl-End", "follow stream"),
        ("mouse wheel", "scroll · click session · click input"),
        ("F1", "this help"),
        ("F2", "engine log pane (⇧PgUp/PgDn scrolls it)"),
        ("F3", "markdown source ⇄ rendered"),
        ("F4", "live preview of what you're typing"),
        ("Ctrl-O", "open a link (click one too)"),
        ("Ctrl-Y / Ctrl-⇧Y", "copy last reply / its last code block"),
        ("Alt-Enter", "newline instead of send"),
        (
            "/theme <name>",
            "switch palette (synthwave/classic/amber/mono)",
        ),
        ("/themes", "list all themes incl. your own"),
        ("/art", "show the synthwave scene"),
        (
            "/plan [on|off]",
            "plan mode: research freely, change nothing",
        ),
        (
            "/plan show|tasks",
            "read back the presented plan / the task list",
        ),
        ("/todos /checkpoints", "local session state"),
    ];
    let mut lines: Vec<Line> = Vec::new();
    for (k, d) in rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {k:<16}", k = k),
                Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(d, Style::default().fg(t.text())),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Esc to close",
        Style::default().fg(t.dim()),
    )));
    let popup = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.cyan()))
            .title(Span::styled(
                " keys ",
                Style::default().fg(t.cyan()).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(t.panel())),
    );
    f.render_widget(Clear, area);
    f.render_widget(popup, area);
}

fn draw_confirm(f: &mut ratatui::Frame<'_>, sid: &str, t: &veles_core::theme::Theme) {
    let area = centered_rect(60, 30, f.size());
    let popup = Paragraph::new(vec![
        Line::from(Span::styled(
            format!("Delete session {sid} and all its messages?"),
            Style::default().fg(t.text()).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "  Enter ",
                Style::default().fg(t.red()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("delete · ", Style::default().fg(t.text())),
            Span::styled(
                "Esc / n ",
                Style::default().fg(t.green()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("cancel", Style::default().fg(t.text())),
        ]),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(t.red()))
            .title(Span::styled(
                " delete session ",
                Style::default().fg(t.red()).add_modifier(Modifier::BOLD),
            ))
            .style(Style::default().bg(t.panel())),
    );
    f.render_widget(Clear, area);
    f.render_widget(popup, area);
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

/// TUI-local slash commands (task list, checkpoints, theme, art).
/// `None` means the input is an ordinary prompt for the model.
fn local_command(
    st: &mut UiState,
    agent: &AgentHandle,
    history: &Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
    text: &str,
) -> Option<String> {
    let (head, rest) = match text.split_once(' ') {
        Some((h, r)) => (h, r.trim()),
        None => (text, ""),
    };
    // Editors, not dumps: a bare command whose answer is a choice opens a
    // picker instead of prose. A specific argument (`/model <name>`,
    // `/diff staged`) still reaches the shared dispatcher below.
    match head {
        "/model" if rest.is_empty() => {
            st.picker = Some(model_picker(agent));
            return Some("pick a model (↑/↓, Enter, Esc)".into());
        }
        "/checkpoints" if rest.is_empty() => {
            st.picker = Some(checkpoints_picker(agent));
            return Some("checkpoints — ↑/↓, Enter restores, Esc closes".into());
        }
        "/skills" if rest.is_empty() => {
            st.picker = Some(skills_picker());
            return Some("skills — ↑/↓, Enter reads one, Esc closes".into());
        }
        "/plugins" if rest.is_empty() => {
            st.picker = Some(plugins_picker(agent));
            return Some("plugins — ↑/↓, Enter inspects, Esc closes".into());
        }
        "/hooks" if rest.is_empty() => {
            st.picker = Some(hooks_picker());
            return Some("hooks — ↑/↓, Enter inspects, Esc closes".into());
        }
        "/memory" if rest.is_empty() => {
            st.picker = Some(memory_picker());
            return Some("memory — ↑/↓, Enter reads a store, Esc closes".into());
        }
        "/acl" if rest.is_empty() => {
            st.picker = Some(acl_picker(agent));
            return Some("acl — ↑/↓, Enter inspects (the promote rows run), Esc closes".into());
        }
        "/diff" if rest.is_empty() => {
            st.picker = Some(diff_picker());
            return Some("review — ↑/↓, Enter shows a file's diff, Esc closes".into());
        }
        _ => {}
    }
    // Shared engine-state commands first — one implementation for the
    // REPL, this surface and the gateway. See frontends/commands.rs.
    // `turn_live` is this surface's busy flag: the transcript editors
    // refuse mid-turn instead of racing the running turn.
    {
        let a = cur(agent);
        let mut h = history.lock_ok();
        let mut ctx = super::commands::CommandCtx {
            agent: &a,
            surface: super::commands::Surface::Tui,
            history: Some(&mut *h),
            turn_live: st.busy,
            principal: None,
        };
        if let Some(out) = super::commands::dispatch(&mut ctx, head, rest) {
            drop(a);
            if out.reload {
                let sid = st.session.clone();
                reload_session(st, agent, history, &sid);
            }
            return Some(out.text);
        }
    }
    match head {
        "/files" => {
            let p = rest.trim();
            st.picker = Some(files_picker(if p.is_empty() { "." } else { p }));
            Some("files — ↑/↓, Enter on a dir descends, on a file reads it, Esc closes".into())
        }
        "/log" | "/logs" => {
            st.show_log = !st.show_log;
            st.log_scroll = 0;
            Some(format!(
                "log pane {} (F2) · file: {}",
                if st.show_log { "shown" } else { "hidden" },
                veles_core::logbuf::log_path().display()
            ))
        }
        "/bindings" | "/keymap" => {
            if rest.is_empty() {
                Some(format!(
                    "bindings: {} · /bindings <normie|emacs|vi>",
                    st.keymap.name()
                ))
            } else {
                match veles_core::keymap::Keymap::parse(rest) {
                    Some(k) => {
                        st.keymap = k;
                        st.vi_insert = true;
                        Some(veles_core::i18n::lookup(
                            "bindings %{name}",
                            &[("name", k.name().to_string())],
                        ))
                    }
                    None => Some(veles_core::i18n::lookup(
                        "unknown bindings '%{rest}' · normie|emacs|vi",
                        &[("rest", rest.to_string())],
                    )),
                }
            }
        }
        "/art" => Some(render_art_text(&st.theme)),
        "/help" => {
            // One generated table (RM4.4) — same listing every surface
            // renders, plus the TUI-only rows. See commands::help_entries.
            Some(super::commands::help_text(super::commands::Surface::Tui))
        }
        "/redraw" => {
            st.cache = Default::default();
            Some("redrawn".into())
        }
        "/themes" => {
            let names: Vec<String> = veles_core::theme::all()
                .iter()
                .map(|t| {
                    if t.name == st.theme.name {
                        format!("◆ {} (active)", t.name)
                    } else {
                        format!("  {}", t.name)
                    }
                })
                .collect();
            Some(format!(
                "themes:\n{}\nswitch: /theme <name>",
                names.join("\n")
            ))
        }
        "/theme" => {
            if rest.is_empty() {
                Some(veles_core::i18n::lookup(
                    "current theme: %{name} · /theme <name>",
                    &[("name", st.theme.name.to_string())],
                ))
            } else {
                match veles_core::theme::all()
                    .into_iter()
                    .find(|t| t.name == rest)
                {
                    Some(t) => {
                        st.theme = t;
                        Some(veles_core::i18n::lookup(
                            "theme → %{name}",
                            &[("name", st.theme.name.to_string())],
                        ))
                    }
                    None => Some(veles_core::i18n::lookup(
                        "unknown theme '%{rest}' · /themes to list",
                        &[("rest", rest.to_string())],
                    )),
                }
            }
        }
        _ => None,
    }
}

/// Plain-text rendering of the scene (no colors) — for the `/art`
/// command in the TUI (the transcript is already colored; a
/// monochrome dump reads cleanly).
fn render_art_text(_t: &veles_core::theme::Theme) -> String {
    let mut out = String::new();
    for l in veles_core::art::synthwave_scene() {
        out.push_str(&l.text);
        out.push('\n');
    }
    out
}

/// Art scene as styled ratatui lines, colored by role → theme. Used by
/// the empty-state splash (draw_chat when the transcript is empty).
fn art_scene_spans(t: &veles_core::theme::Theme) -> Vec<Line<'static>> {
    veles_core::art::synthwave_scene()
        .into_iter()
        .map(|l| {
            let col = match l.role {
                veles_core::art::ArtRole::Sky => t.panel(),
                veles_core::art::ArtRole::Horizon => t.dim(),
                veles_core::art::ArtRole::Sun => t.pink(),
                veles_core::art::ArtRole::Slice => t.bg(),
                veles_core::art::ArtRole::Mountain => t.purple(),
                veles_core::art::ArtRole::Grid => t.cyan(),
                veles_core::art::ArtRole::Accent => t.gold(),
            };
            Line::from(Span::styled(l.text.clone(), Style::default().fg(col)))
        })
        .collect()
}

/// Handles headings, code fences, inline bold/italic/code. Not a full
/// parser — terminal surfaces need styling, not semantics.
fn md_lines(
    text: &str,
    th: &veles_core::theme::Theme,
    table_width: usize,
) -> Vec<Vec<Span<'static>>> {
    md_lines_tracked(text, th, table_width)
        .into_iter()
        .map(|(s, _)| s)
        .collect()
}

/// Same rendering, plus the link column ranges per logical line.
fn md_lines_tracked(
    text: &str,
    th: &veles_core::theme::Theme,
    table_width: usize,
) -> Vec<(Vec<Span<'static>>, Vec<LinkSpan>)> {
    let mut out: Vec<(Vec<Span<'static>>, Vec<LinkSpan>)> = Vec::new();
    let mut in_fence = false;
    let source: Vec<&str> = text.lines().collect();
    let mut i = 0usize;
    while i < source.len() {
        let line = source[i];
        let t = line.trim_start();
        if t.starts_with("```") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            out.push((
                vec![Span::styled(
                    format!("  {line}"),
                    Style::default().fg(th.gold()),
                )],
                Vec::new(),
            ));
            i += 1;
            continue;
        }
        if line.contains('|') {
            if let Some((rows, consumed)) = mdtable::try_render(&source, i, th, table_width) {
                out.extend(rows);
                i += consumed;
                continue;
            }
        }
        if let Some(rest) = t.strip_prefix("# ") {
            out.push((
                vec![Span::styled(
                    rest.to_string(),
                    Style::default().fg(th.pink()).add_modifier(Modifier::BOLD),
                )],
                Vec::new(),
            ));
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("## ") {
            out.push((
                vec![Span::styled(
                    rest.to_string(),
                    Style::default()
                        .fg(th.purple())
                        .add_modifier(Modifier::BOLD),
                )],
                Vec::new(),
            ));
            i += 1;
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ") {
            out.push((
                vec![Span::styled(
                    format!("  {rest}"),
                    Style::default().fg(th.purple()),
                )],
                Vec::new(),
            ));
            i += 1;
            continue;
        }
        // Links live in ordinary prose and in bullets, so both go
        // through the link-aware path.
        let (prefix, body): (Vec<Span<'static>>, &str) =
            if t.starts_with("- ") || t.starts_with("* ") {
                (
                    vec![Span::styled("  ▸ ", Style::default().fg(th.cyan()))],
                    &t[2..],
                )
            } else {
                (Vec::new(), line)
            };
        let (clean, labels) = extract_links(body);
        let mut spans = prefix.clone();
        spans.extend(inline_spans(&clean, th));
        let mut links = Vec::new();
        if !labels.is_empty() {
            let prefix_w: usize = prefix
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            let rendered: String = spans
                .iter()
                .skip(prefix.len())
                .map(|s| s.content.as_ref())
                .collect();
            links = locate_links(&rendered, &labels)
                .into_iter()
                .map(|mut l| {
                    l.start += prefix_w;
                    l.end += prefix_w;
                    l
                })
                .collect();
            // Underline what is clickable so it looks like a link.
            spans = style_links(spans, prefix.len(), &links, prefix_w, th);
        }
        out.push((spans, links));
        i += 1;
    }
    out
}

/// Restyle the character runs covered by links (cyan + underline).
/// Spans are rebuilt per character in that range only.
fn style_links(
    spans: Vec<Span<'static>>,
    skip: usize,
    links: &[LinkSpan],
    prefix_w: usize,
    th: &veles_core::theme::Theme,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;
    let link_style = Style::default()
        .fg(th.cyan())
        .add_modifier(Modifier::UNDERLINED);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = prefix_w;
    for (i, span) in spans.into_iter().enumerate() {
        if i < skip {
            out.push(span);
            continue;
        }
        for c in span.content.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(1);
            let inside = links.iter().any(|l| col >= l.start && col < l.end);
            out.push(Span::styled(
                c.to_string(),
                if inside { link_style } else { span.style },
            ));
            col += w;
        }
    }
    out
}

/// A link found while rendering: where it sits in the logical line (in
/// display columns) and where it points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkSpan {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

/// Pull `[label](url)` and bare `http(s)://…` out of a line, returning
/// the text with labels substituted for the full markdown, plus the
/// (label, url) pairs in order.
///
/// Columns are deliberately NOT computed here: `inline_spans` still has
/// to strip `**`/`*`/`` ` `` markers afterwards, which shifts every
/// column to its right. The positions are located against the finished
/// text instead (`locate_links`), which is exact and needs no
/// bookkeeping.
fn extract_links(line: &str) -> (String, Vec<(String, String)>) {
    static MD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BARE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let md = MD.get_or_init(|| regex::Regex::new(r"\[([^\]\n]+)\]\(([^)\n\s]+)\)").unwrap());
    let bare = BARE.get_or_init(|| regex::Regex::new(r#"https?://[^\s<>()\[\]"']+"#).unwrap());

    let mut out = String::new();
    let mut links = Vec::new();
    let mut rest = line;
    loop {
        let m_md = md.find(rest);
        let m_bare = bare.find(rest);
        // Whichever construct starts first wins; a bare URL inside a
        // markdown link is part of that link, and md always starts at
        // the `[` which precedes it.
        let pick = match (m_md, m_bare) {
            (Some(a), Some(b)) => {
                if a.start() <= b.start() {
                    Some((a.start(), a.end(), true))
                } else {
                    Some((b.start(), b.end(), false))
                }
            }
            (Some(a), None) => Some((a.start(), a.end(), true)),
            (None, Some(b)) => Some((b.start(), b.end(), false)),
            (None, None) => None,
        };
        let Some((s, e, is_md)) = pick else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..s]);
        let chunk = &rest[s..e];
        let (label, url) = if is_md {
            let c = md.captures(chunk).unwrap();
            (c[1].to_string(), c[2].to_string())
        } else {
            (chunk.to_string(), chunk.to_string())
        };
        out.push_str(&label);
        links.push((label, url));
        rest = &rest[e..];
    }
    (out, links)
}

/// Find each link label in the finished line and turn it into a column
/// range. Labels are matched in order from a moving cursor, so repeated
/// labels ("here", "here") map to their own occurrences.
fn locate_links(rendered: &str, labels: &[(String, String)]) -> Vec<LinkSpan> {
    use unicode_width::UnicodeWidthStr;
    let mut out = Vec::new();
    let mut from = 0usize; // byte cursor into `rendered`
    for (label, url) in labels {
        if label.is_empty() {
            continue;
        }
        let Some(rel) = rendered.get(from..).and_then(|s| s.find(label.as_str())) else {
            continue;
        };
        let at = from + rel;
        let start = UnicodeWidthStr::width(&rendered[..at]);
        let end = start + UnicodeWidthStr::width(label.as_str());
        out.push(LinkSpan {
            start,
            end,
            url: url.clone(),
        });
        from = at + label.len();
    }
    out
}

/// Inline **bold**, *italic*, `code` → styled spans. Simple state
/// machine; unmatched markers render literally (never panic).
fn inline_spans(line: &str, th: &veles_core::theme::Theme) -> Vec<Span<'static>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    let mut bold = false;
    let mut italic = false;
    let mut code = false;
    let flush = |spans: &mut Vec<Span>, buf: &mut String, bold: bool, italic: bool, code: bool| {
        if buf.is_empty() {
            return;
        }
        let mut style = if code {
            Style::default().fg(th.gold())
        } else if bold {
            Style::default().fg(th.text()).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.text())
        };
        if italic && !code && !bold {
            style = style.add_modifier(Modifier::ITALIC);
        }
        spans.push(Span::styled(std::mem::take(buf), style));
    };
    while i < chars.len() {
        let c = chars[i];
        if !code && c == '*' {
            if i + 1 < chars.len() && chars[i + 1] == '*' {
                flush(&mut spans, &mut buf, bold, italic, code);
                bold = !bold;
                i += 2;
                continue;
            }
            flush(&mut spans, &mut buf, bold, italic, code);
            italic = !italic;
            i += 1;
            continue;
        }
        if c == '`' {
            flush(&mut spans, &mut buf, bold, italic, code);
            code = !code;
            i += 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(&mut spans, &mut buf, bold, italic, code);
    spans
}

#[cfg(test)]
mod tui_tests {
    use super::*;

    fn plain(line: &str) -> String {
        inline_spans(line, &veles_core::theme::synthwave())
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    // ── the ask popup's keyboard model (2026-09-26 parity) ──────────

    fn mq_req() -> veles_core::ask::AskRequest {
        veles_core::ask::AskRequest::multi_question(
            "s1",
            veles_core::ask::AskKind::Clarify,
            vec![
                veles_core::ask::AskQuestion::new(
                    "q1",
                    "theme?",
                    vec![
                        veles_core::ask::AskChoice::new("c1", "synthwave"),
                        veles_core::ask::AskChoice::new("c2", "amber"),
                    ],
                ),
                veles_core::ask::AskQuestion::new(
                    "q2",
                    "art?",
                    vec![
                        veles_core::ask::AskChoice::new("c1", "yes"),
                        veles_core::ask::AskChoice::new("c2", "no"),
                    ],
                )
                .with_multi(true),
            ],
        )
    }

    /// The row model: per-question choices + an Other row each, header
    /// rows for grouping. Cursor indices must match draw_ask's
    /// rendering (the two walk the same list).
    #[test]
    fn ask_rows_lay_out_choices_and_other_per_question() {
        let rows = ask_rows(&mq_req());
        // q1: header + 2 choices + Other; q2: header + 2 choices + Other.
        assert_eq!(rows.len(), 8, "{rows:?}");
        assert!(matches!(&rows[0], AskRow::Header { text } if text == "theme?"));
        assert!(matches!(&rows[1], AskRow::Choice { addr, .. } if addr == "q1:c1"));
        assert!(matches!(&rows[3], AskRow::Other { qk } if qk == "q1"));
        assert!(matches!(&rows[5], AskRow::Choice { addr, .. } if addr == "q2:c1"));
        // A single-question ask has no header rows and no Other when
        // it has no choices.
        let single = veles_core::ask::AskRequest::new(
            "s1",
            veles_core::ask::AskKind::Clarify,
            "open question?",
            vec![],
        );
        assert!(ask_rows(&single).is_empty());
        // A FREE-TEXT question inside a multi-question ask carries its
        // own text row — that is its answer field (one blob spread
        // over several questions is the guessing parse_reply refuses).
        let mixed = veles_core::ask::AskRequest::multi_question(
            "s1",
            veles_core::ask::AskKind::Clarify,
            vec![
                veles_core::ask::AskQuestion::new(
                    "q1",
                    "pick?",
                    vec![veles_core::ask::AskChoice::new("c1", "a")],
                ),
                veles_core::ask::AskQuestion::new("q2", "say?", vec![]),
            ],
        );
        let rows = ask_rows(&mixed);
        // q1: header + choice + Other; q2: header + text row (index 4).
        assert!(
            matches!(&rows[4], AskRow::Other { qk } if qk == "q2"),
            "the free-text question has a text row: {rows:?}"
        );
        assert_eq!(rows.len(), 5, "{rows:?}");
    }

    /// Space on a choice row toggles when the ask is multi (or
    /// multi-question), and ANSWERS instantly on a plain single-choice
    /// ask — the two regimes the key arm switches between.
    #[test]
    fn space_toggles_in_multi_mode_and_answers_in_instant_mode() {
        use veles_core::ask::AskAnswer;
        let toggles = std::collections::BTreeSet::new();
        // Multi-question: Space toggles. Rows: 0 hdr, 1 q1:c1, … 5 q2:c1.
        match ask_key(&mq_req(), 1, &toggles, &KeyCode::Char(' ')) {
            AskKeyEffect::Toggle { addr, radio } => {
                assert_eq!(addr, "q1:c1");
                assert!(radio, "q1 is single-select: radio");
            }
            other => panic!("expected Toggle, got {other:?}"),
        }
        // A multi question's row: radio off.
        match ask_key(&mq_req(), 5, &toggles, &KeyCode::Char(' ')) {
            AskKeyEffect::Toggle { addr, radio } => {
                assert_eq!(addr, "q2:c1");
                assert!(!radio, "q2 is multi: checkbox");
            }
            other => panic!("expected Toggle, got {other:?}"),
        }
        // Plain single-choice: Space ANSWERS.
        let plain_req = veles_core::ask::AskRequest::new(
            "s1",
            veles_core::ask::AskKind::Clarify,
            "pick",
            vec![veles_core::ask::AskChoice::new("c1", "a")],
        );
        match ask_key(&plain_req, 0, &toggles, &KeyCode::Char(' ')) {
            AskKeyEffect::Answer(a) => assert_eq!(a.first(), Some("c1")),
            other => panic!("expected Answer, got {other:?}"),
        }
        let _ = AskAnswer::default(); // silence unused-import style noise
    }

    /// An approval (and a failover) keeps the key model it always had.
    /// The parity round gave it the clarify cursor: Space — the key
    /// that scrolls a long plan — ANSWERED the highlighted row, which
    /// is "allow once", and Enter on a freshly popped approval approved
    /// it (a composer Enter racing the popup was enough). Neither may
    /// decide an approval: only an explicit 1-4 / y / n does, and Esc
    /// denies. No Other row either — an approval's free text is the
    /// desktop's amend, not a typed answer.
    #[test]
    fn an_approval_answers_only_on_an_explicit_key() {
        use veles_core::ask::{AskKind, AskRequest, CHOICE_DENY, CHOICE_ONCE};
        let toggles = std::collections::BTreeSet::new();
        let other = std::collections::BTreeMap::new();
        let plan = (0..60)
            .map(|i| format!("step {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        for kind in [AskKind::Approval, AskKind::Failover] {
            let req = AskRequest::new(
                "s1",
                kind,
                "run it?",
                veles_core::ask::approval_choices(true),
            )
            .with_detail(plan.clone());
            assert!(
                matches!(
                    ask_key(&req, 0, &toggles, &KeyCode::Char(' ')),
                    AskKeyEffect::ScrollDown
                ),
                "{kind:?}: Space scrolls the plan"
            );
            assert!(
                matches!(
                    ask_key(&req, 0, &toggles, &KeyCode::Enter),
                    AskKeyEffect::None
                ),
                "{kind:?}: Enter decides nothing"
            );
            assert!(
                commit_ask(&req, 0, &toggles, &other, "").is_none(),
                "{kind:?}: nor does a commit reaching it another way"
            );
            assert!(
                !ask_rows(&req)
                    .iter()
                    .any(|r| matches!(r, AskRow::Other { .. })),
                "{kind:?}: no Other row"
            );
        }
        let req = AskRequest::new(
            "s1",
            AskKind::Approval,
            "run it?",
            veles_core::ask::approval_choices(true),
        );
        match ask_key(&req, 0, &toggles, &KeyCode::Char('1')) {
            AskKeyEffect::Answer(a) => assert_eq!(a.first(), Some(CHOICE_ONCE)),
            other => panic!("1 answers allow-once, got {other:?}"),
        }
        match ask_key(&req, 0, &toggles, &KeyCode::Char('n')) {
            AskKeyEffect::Answer(a) => assert_eq!(a.first(), Some(CHOICE_DENY)),
            other => panic!("n denies, got {other:?}"),
        }
    }

    /// Every character types into a text answer — the plain input of an
    /// open question and an Other row alike. The cursor model took `j`,
    /// `k` and the digits for navigation first, so "keep 3 jobs" arrived
    /// as "eep  obs". Only the arrows move the cursor off a text row.
    #[test]
    fn every_character_types_into_a_text_answer() {
        let toggles = std::collections::BTreeSet::new();
        let open = veles_core::ask::AskRequest::new(
            "s1",
            veles_core::ask::AskKind::Clarify,
            "open?",
            vec![],
        );
        // mq_req row 3 is q1's Other row.
        for (req, sel) in [(open, 0usize), (mq_req(), 3)] {
            for c in "keep 3 jobs".chars() {
                match ask_key(&req, sel, &toggles, &KeyCode::Char(c)) {
                    AskKeyEffect::Type(t) => assert_eq!(t, c),
                    other => panic!("{c:?} on row {sel} must type, got {other:?}"),
                }
            }
        }
        assert!(matches!(
            ask_key(&mq_req(), 3, &toggles, &KeyCode::Up),
            AskKeyEffect::SetSel(2)
        ));
        // On a CHOICE row the letters still navigate and digits jump.
        assert!(matches!(
            ask_key(&mq_req(), 2, &toggles, &KeyCode::Char('k')),
            AskKeyEffect::SetSel(1)
        ));
    }

    /// Enter assembles the answer: toggled selections across questions
    /// become one per_question answer; the Other row's text rides
    /// along as free text.
    #[test]
    fn commit_assembles_toggles_and_other_text() {
        let req = mq_req();
        let mut toggles = std::collections::BTreeSet::new();
        toggles.insert("q1:c2".to_string());
        toggles.insert("q2:c1".to_string());
        toggles.insert("q2:c2".to_string());
        let mut other = std::collections::BTreeMap::new();
        other.insert("q1".to_string(), "actually both".to_string());
        let a = commit_ask(&req, 0, &toggles, &other, "").unwrap();
        assert_eq!(a.per_question.len(), 2, "{a:?}");
        assert_eq!(a.per_question["q1"].selected, vec!["c2"]);
        assert_eq!(
            a.per_question["q1"].free_text.as_deref(),
            Some("actually both")
        );
        assert_eq!(a.per_question["q2"].selected, vec!["c1", "c2"]);
    }

    /// Enter with nothing toggled picks the CURSOR's row (the
    /// keyboard default-button equivalent), one question only.
    #[test]
    fn commit_with_nothing_toggled_answers_the_cursor_row() {
        let req = mq_req();
        let toggles = std::collections::BTreeSet::new();
        let other = std::collections::BTreeMap::new();
        // Row layout: 0 hdr q1, 1 q1:c1, 2 q1:c2, 3 Other q1,
        // 4 hdr q2, 5 q2:c1, 6 q2:c2, 7 Other q2. Cursor on 6.
        let a = commit_ask(&req, 6, &toggles, &other, "").unwrap();
        assert_eq!(a.per_question.len(), 1);
        assert_eq!(a.per_question["q2"].selected, vec!["c2"]);
        // A header row commits nothing — Enter is a no-op there.
        assert!(commit_ask(&req, 4, &toggles, &other, "").is_none());
    }

    /// Typing lands in the Other buffer when the cursor is on an
    /// Other row, and in the plain input for a no-choices ask.
    #[test]
    fn typing_targets_the_other_row_or_the_plain_input() {
        let req = mq_req();
        // Row 3 = q1's Other row; row 1 = a choice (no typing).
        assert_eq!(ask_typing_target(&req, 3), TypingTarget::Other("q1".into()));
        assert_eq!(ask_typing_target(&req, 1), TypingTarget::Nowhere);
        let open = veles_core::ask::AskRequest::new(
            "s1",
            veles_core::ask::AskKind::Clarify,
            "open?",
            vec![],
        );
        assert_eq!(ask_typing_target(&open, 0), TypingTarget::Plain);
    }

    /// USER 2026-10-10: notes on asks were dropped on every surface. The
    /// TUI answers an approval on one key, so its note is a line of its
    /// own, and the key picked afterwards carries it.
    #[test]
    fn an_approval_key_carries_the_typed_note_and_a_clarify_is_untouched() {
        use veles_core::ask::{AskAnswer, AskChoice, AskKind, AskRequest};
        let appr = AskRequest::new(
            "s",
            AskKind::Approval,
            "run it?",
            veles_core::ask::approval_choices(true),
        );
        assert!(takes_note(&appr));
        let a = with_note(&appr, AskAnswer::choice("once"), "  keep the logs  ");
        assert_eq!(a.first(), Some("once"));
        assert_eq!(a.free_text.as_deref(), Some("keep the logs"));
        let a = with_note(&appr, AskAnswer::choice("once"), "   ");
        assert!(a.free_text.is_none(), "an empty note adds nothing");
        let amend = AskAnswer {
            selected: vec!["once".into()],
            free_text: Some("amend: {}".into()),
            ..Default::default()
        };
        assert_eq!(
            with_note(&appr, amend, "note").free_text.as_deref(),
            Some("amend: {}"),
            "an answer that brought its own text keeps it"
        );
        let clar = AskRequest::new(
            "s",
            AskKind::Clarify,
            "which?",
            vec![AskChoice::new("c1", "a")],
        );
        assert!(!takes_note(&clar));
        assert!(with_note(&clar, AskAnswer::choice("c1"), "x")
            .free_text
            .is_none());
    }

    #[test]
    fn the_note_line_shows_while_it_has_the_keys() {
        use ratatui::backend::TestBackend;
        let req = veles_core::ask::AskRequest::new(
            "s",
            veles_core::ask::AskKind::Approval,
            "Plan ready for approval",
            vec![
                veles_core::ask::AskChoice::new("approve", "approve and start work"),
                veles_core::ask::AskChoice::new("keep-planning", "keep planning"),
            ],
        );
        let toggles = std::collections::BTreeSet::new();
        let other = std::collections::BTreeMap::new();
        let mut term = Terminal::new(TestBackend::new(100, 30)).expect("test terminal");
        term.draw(|f| {
            draw_ask(
                f,
                &req,
                "drop step 2",
                true,
                0,
                0,
                &toggles,
                &other,
                &veles_core::theme::synthwave(),
            );
        })
        .expect("draw");
        let text = term.backend().to_string();
        assert!(text.contains("note: drop step 2"), "{text}");
        assert!(text.contains("Enter sends the note alone"), "{text}");
    }

    /// The cursor highlight must sit on the row `ask_rows` says it
    /// does — draw_ask walks its OWN loop, and forgetting to count a
    /// header row made the highlight drift one row per question (the
    /// cursor SAID q2:c1 while the box glowed q1's Other row). This
    /// renders for real and checks the ❯ lands next to the choice the
    /// flat model names. Mutation: deleting the header `row_idx += 1`
    /// in draw_ask turns this red.
    #[test]
    fn the_cursor_highlight_lands_on_the_named_row() {
        use ratatui::backend::TestBackend;
        let req = mq_req();
        // Row 6 in the flat model = q2:c2 ("no").
        let toggles = std::collections::BTreeSet::new();
        let other = std::collections::BTreeMap::new();
        let mut term = Terminal::new(TestBackend::new(100, 30)).expect("test terminal");
        term.draw(|f| {
            draw_ask(
                f,
                &req,
                "",
                false,
                0,
                6,
                &toggles,
                &other,
                &veles_core::theme::synthwave(),
            );
        })
        .expect("draw");
        let text = term.backend().to_string();
        let cursor_line = text
            .lines()
            .find(|l| l.contains("\u{276f}") && l.contains("no"))
            .unwrap_or_else(|| panic!("cursor not on the 'no' row:\n{text}"));
        assert!(
            cursor_line.contains("2)")
                && cursor_line.contains("no")
                && !cursor_line.contains("art"),
            "the cursor row is q2's second choice: {cursor_line}"
        );
        assert!(
            !text
                .lines()
                .any(|l| l.contains("\u{276f}") && l.contains("amber")),
            "the highlight drifted onto q1's row:\n{text}"
        );
    }

    /// Guards the fix for "the TUI pins a core while scrolling".
    ///
    /// Every frame used to re-parse the whole transcript's markdown and
    /// clone the result; the tick thread redraws 8×/s regardless of
    /// input, so idling on a long session burned a CPU and each wheel
    /// event paid the full cost again. Rendering is now per-message and
    /// cached, so a frame that changes nothing costs a slice.
    #[test]
    fn cached_frames_are_far_cheaper_than_rendering() {
        use std::time::Instant;
        let th = veles_core::theme::synthwave();
        let body = "Some **bold** prose with `code`, a [link](https://example.com/x) \
and a bare https://example.com/y, plus a list:\n- one\n- two\n\n```rust\nfn main() {}\n```\n";
        let mut tr = Transcript::new();
        for i in 0..400 {
            if i % 2 == 0 {
                tr.push(TEvent::Delta(format!("{body}\n(message {i})")));
                tr.close_all();
            } else {
                tr.push(TEvent::User {
                    text: format!("{body}\n(message {i})"),
                    time: "12:34".into(),
                    injection: None,
                });
            }
        }
        let msgs = &tr.blocks;

        // What a frame used to cost: render everything.
        let t0 = Instant::now();
        let rendered: Vec<MsgRender> = msgs
            .iter()
            .map(|m| render_msg(m, &th, 100, false))
            .collect();
        let full = t0.elapsed();

        // What a frame costs now: reuse the cache, clone ~40 rows.
        let total: usize = rendered.iter().map(|r| r.lines.len()).sum();
        let t1 = Instant::now();
        for off in 0..50usize {
            let want = off..off + 40;
            let mut view: Vec<Line<'static>> = Vec::with_capacity(40);
            let mut cursor = 0usize;
            for c in &rendered {
                if cursor + c.lines.len() > want.start && cursor < want.end {
                    for (i, l) in c.lines.iter().enumerate() {
                        if want.contains(&(cursor + i)) {
                            view.push(l.clone());
                        }
                    }
                }
                cursor += c.lines.len();
            }
            assert!(!view.is_empty());
        }
        let cached = t1.elapsed() / 50;

        println!(
            "render-all {full:?} for {} rows · cached frame {cached:?} ({}× cheaper)",
            total,
            full.as_nanos() / cached.as_nanos().max(1)
        );
        assert!(
            cached * 10 < full,
            "a cached frame ({cached:?}) must be far cheaper than rendering \
             the transcript ({full:?})"
        );
    }

    #[test]
    fn cache_key_catches_growth_and_kind_changes() {
        let th = veles_core::theme::synthwave();
        let mut tr = Transcript::new();
        tr.push(TEvent::Delta("hi".into()));
        let r = render_msg(&tr.blocks[0], &th, 40, false);
        assert_eq!(r.len, 2);
        assert_eq!(r.kind, Kind::Reply);
        // A streaming delta grows the text — the length key notices.
        let mut grown = tr.clone();
        grown.push(TEvent::Delta(" there".into()));
        assert_ne!(render_msg(&grown.blocks[0], &th, 40, false).len, r.len);
        // A collapsed/expanded flip changes the render without changing
        // the source — the key must carry it.
        let mut flip = tr.clone();
        flip.blocks[0].kind = Kind::Reasoning;
        let collapsed = render_msg(&flip.blocks[0], &th, 40, false);
        flip.blocks[0].collapsed = !flip.blocks[0].collapsed;
        let expanded = render_msg(&flip.blocks[0], &th, 40, false);
        assert_ne!(collapsed.collapsed, expanded.collapsed);
    }

    #[test]
    fn search_matches_message_text_and_cycles() {
        let mut tr = Transcript::new();
        for (text, kind_reply) in [
            ("how do I parse TOML", false),
            ("use the toml crate", true),
            ("unrelated", true),
            ("TOML again", true),
        ] {
            if kind_reply {
                tr.push(TEvent::Reply {
                    text: text.into(),
                    time: String::new(),
                    reasoning: None,
                });
            } else {
                tr.push(TEvent::User {
                    text: text.into(),
                    time: String::new(),
                    injection: None,
                });
            }
        }
        let mut s = SearchState {
            query: "toml".into(),
            ..Default::default()
        };
        s.recompute(&tr);
        // Case-insensitive, in transcript order.
        assert_eq!(s.hits, vec![0, 1, 3]);
        assert_eq!(s.current(), Some(0));
        assert_eq!(s.step(true), Some(1));
        assert_eq!(s.step(true), Some(3));
        assert_eq!(s.step(true), Some(0), "wraps around");
        assert_eq!(s.step(false), Some(3), "and backwards");

        // Narrowing the query resets the cursor rather than dangling.
        s.query = "again".into();
        s.recompute(&tr);
        assert_eq!(s.hits, vec![3]);
        assert_eq!(s.at, 0);

        // No query = no matches (not "everything").
        s.query.clear();
        s.recompute(&tr);
        assert!(s.hits.is_empty());
        assert_eq!(s.step(true), None);
    }

    #[test]
    fn osc52_frames_base64_between_the_right_markers() {
        use base64::Engine;
        // The sequence itself is what the terminal parses; assert the
        // framing rather than the side effect (writing to a real
        // stdout is not something a unit test should do).
        let payload = "hello\nworld";
        let b64 = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
        let seq = format!("\x1b]52;c;{b64}\x07");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(
                seq.trim_start_matches("\x1b]52;c;")
                    .trim_end_matches('\x07'),
            )
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), payload);
    }

    #[test]
    fn last_code_block_picks_the_last_fence() {
        let text = "intro\n```sh\nfirst\n```\nmiddle\n```rust\nfn main() {}\nlet x = 1;\n```\nend";
        assert_eq!(last_code_block(text).unwrap(), "fn main() {}\nlet x = 1;");
        assert!(last_code_block("no fences here").is_none());
        // An unterminated fence is not a block (nothing was closed).
        assert!(last_code_block("```\ndangling").is_none());
    }

    #[test]
    fn links_are_extracted_and_located() {
        let (clean, labels) = extract_links("see [the docs](https://x.dev/a) now");
        assert_eq!(clean, "see the docs now");
        assert_eq!(labels, vec![("the docs".into(), "https://x.dev/a".into())]);
        let spans = locate_links(&clean, &labels);
        assert_eq!(spans.len(), 1);
        // "see " is 4 columns wide; the label is 8.
        assert_eq!((spans[0].start, spans[0].end), (4, 12));
        assert_eq!(spans[0].url, "https://x.dev/a");
    }

    #[test]
    fn bare_urls_autolink_and_markdown_wins_over_them() {
        let (clean, labels) = extract_links("go to https://x.dev/bare ok");
        assert_eq!(clean, "go to https://x.dev/bare ok");
        assert_eq!(labels.len(), 1);
        assert_eq!(labels[0].1, "https://x.dev/bare");
        // The URL inside a markdown link must not also autolink.
        let (clean, labels) = extract_links("[a](https://x.dev/1) and https://x.dev/2");
        assert_eq!(clean, "a and https://x.dev/2");
        assert_eq!(labels.len(), 2);
        assert_eq!(labels[0].1, "https://x.dev/1");
        assert_eq!(labels[1].1, "https://x.dev/2");
    }

    #[test]
    fn repeated_labels_map_to_their_own_occurrences() {
        let (clean, labels) = extract_links("[here](https://x.dev/1) then [here](https://x.dev/2)");
        let spans = locate_links(&clean, &labels);
        assert_eq!(spans.len(), 2);
        assert!(spans[0].start < spans[1].start, "distinct positions");
        assert_eq!(spans[0].url, "https://x.dev/1");
        assert_eq!(spans[1].url, "https://x.dev/2");
    }

    #[test]
    fn link_columns_survive_inline_marker_stripping() {
        // `**bold**` loses 4 columns of markers before the link, so a
        // column computed on the pre-render text would be wrong.
        let th = veles_core::theme::synthwave();
        let rows = md_lines_tracked("**hi** [go](https://x.dev/z)", &th, 120);
        assert_eq!(rows.len(), 1);
        let (spans, links) = &rows[0];
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hi go");
        assert_eq!(links.len(), 1);
        let (s, e) = (links[0].start, links[0].end);
        assert_eq!(&text[s..e], "go", "range indexes the rendered text");
    }

    #[test]
    fn wrap_tracks_source_columns_for_hit_testing() {
        let line = Line::from(vec![Span::raw("abcdefghij")]);
        let rows = wrap_line_tracked(line, 4);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].1, 0..4);
        assert_eq!(rows[1].1, 4..8);
        assert_eq!(rows[2].1, 8..10);
        // Same text as the plain wrapper produces.
        let plain: Vec<String> = rows
            .iter()
            .map(|(l, _)| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(plain, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn only_http_urls_are_openable() {
        // NOTE: this tests the POLICY only. It must never call
        // `open_url`, which spawns a real browser — an earlier version
        // of this test did exactly that and opened the developer's
        // browser on every `cargo test` run.
        assert!(url_is_openable("file:///etc/passwd").is_err());
        assert!(url_is_openable("javascript:alert(1)").is_err());
        assert!(url_is_openable("vscode://file/etc/passwd").is_err());
        assert!(url_is_openable("https://example.com/x").is_ok());
        assert!(url_is_openable("http://example.com/x").is_ok());
    }

    #[test]
    fn inline_bold_code_roundtrip_text() {
        assert_eq!(plain("a **b** `c` d"), "a b c d");
        assert_eq!(plain("*it* and **bo**"), "it and bo");
    }

    #[test]
    fn headings_and_fences() {
        let lines = md_lines(
            "# Title\n```\nx_y * z\n```\nend",
            &veles_core::theme::synthwave(),
            120,
        );
        assert_eq!(lines.len(), 3); // heading + fence body + end
                                    // fence body preserved byte-exact (no markdown mangling)
        let body: String = lines[1].iter().map(|s| s.content.as_ref()).collect();
        assert!(body.contains("x_y * z"));
    }

    #[test]
    fn unmatched_markers_never_panic() {
        let lines = md_lines(
            "dangling ** and ` and *",
            &veles_core::theme::synthwave(),
            120,
        );
        assert!(!lines.is_empty());
    }

    #[test]
    fn bullets_render_with_marker() {
        let lines = md_lines("- one\n- two", &veles_core::theme::synthwave(), 120);
        assert_eq!(lines.len(), 2);
        let first: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(first, "  ▸ one");
    }

    #[test]
    fn gfm_tables_have_aligned_columns_and_inline_links() {
        let th = veles_core::theme::synthwave();
        let rows = md_lines_tracked("| Name | Result | State |\n|:---|:---:|---:|\n| Ada | [works](https://x.dev/a) | yes |", &th, 120);
        let texts: Vec<String> = rows
            .iter()
            .map(|(spans, _)| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(
            texts.len(),
            3,
            "header, separator, and one body row: {texts:?}"
        );
        assert!(
            texts[0].contains("Name") && texts[0].contains("Result") && texts[0].contains("State")
        );
        assert!(
            texts[1].chars().all(|c| c == '─' || c == '┼'),
            "rule not raw pipes: {texts:?}"
        );
        let _body = &texts[2];
        let _header = &texts[0];
        // The result column is centered (|:---:|): the shorter body
        // cell gets the extra odd space, while the neighboring columns
        // keep the same starts as their header.
        assert_eq!(
            texts[2], "Ada   works     yes",
            "not raw table delimiters: {texts:?}"
        );
        assert_eq!(
            texts[2].find("yes").map(|i| i + "yes".len()),
            texts[0].find("State").map(|i| i + "State".len()),
            "right-aligned state boundary: {texts:?}"
        );
        assert_eq!(
            texts[2].find("Ada"),
            texts[0].find("Name"),
            "name column: {texts:?}"
        );
        assert!(!texts[2].contains('|'));
        assert_eq!(rows[2].1.len(), 1, "table links stay clickable");
    }

    #[test]
    fn long_and_multiline_cells_keep_the_next_column() {
        let th = veles_core::theme::synthwave();
        let rows = md_lines_tracked(
            "| # | Result | State |\n|---|---|---|\n| 1 | title<br>body | ready |",
            &th,
            120,
        );
        let texts: Vec<String> = rows
            .iter()
            .map(|(spans, _)| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(texts[2].find("ready"), texts[0].find("State"), "{texts:?}");
        assert_eq!(texts[3].find("body"), texts[0].find("Result"), "{texts:?}");
        assert!(
            !texts.iter().any(|s| s.contains("State:")),
            "must not become a prose record: {texts:?}"
        );
    }

    #[test]
    fn a_long_cell_wraps_without_losing_column_starts() {
        let th = veles_core::theme::synthwave();
        let rows = md_lines_tracked("| Name | Notes |\n|---|---|\n| Ada | one two three four five six seven eight nine ten |", &th, 36);
        let texts: Vec<String> = rows
            .iter()
            .map(|(spans, _)| spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(texts.len() > 4, "a long cell must wrap: {texts:?}");
        assert_eq!(
            texts[0], "Name  Notes",
            "the column labels stay whole: {texts:?}"
        );
        for (row, text) in texts.iter().enumerate().skip(2) {
            for word in [
                "Ada", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
                "ten",
            ] {
                if let Some(at) = text.find(word) {
                    let expected = if word == "Ada" {
                        texts[0].find("Name")
                    } else {
                        texts[0].find("Notes")
                    };
                    assert_eq!(Some(at), expected, "row {row}: {texts:?}");
                }
            }
        }
    }

    #[test]
    fn a_pipe_in_a_fence_is_not_a_table() {
        let lines = md_lines(
            "```\n| a | b |\n|---|---|\n```",
            &veles_core::theme::synthwave(),
            120,
        );
        assert_eq!(lines.len(), 2);
        let body: String = lines[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(body.contains("| a | b |"));
    }

    #[test]
    fn truncate_keeps_ellipsis() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
    }

    #[test]
    fn caret_boundaries_are_char_safe() {
        let s = "héllo";
        assert_eq!(prev_boundary(s, 3), 1); // é is 2 bytes; caret at 'l'
        assert_eq!(next_boundary(s, 1), 3);
    }

    #[test]
    fn kill_word_stops_at_space() {
        // bash Ctrl-W semantics: kills back to (and including) the
        // last whitespace — "/tmp/x" here, not just "x".
        let mut s = "rm -rf /tmp/x".to_string();
        let caret = s.len();
        let new = kill_word(&mut s, caret);
        assert_eq!(s, "rm -rf ");
        assert_eq!(new, 7);
        let new2 = kill_word(&mut s, new);
        assert_eq!(s, "rm ");
        assert_eq!(new2, 3);
    }

    #[test]
    fn end_turn_merges_streamed_reply() {
        let mut st = UiState::new("s1".into());
        st.emit(TEvent::User {
            text: "hi".into(),
            time: "12:00".into(),
            injection: None,
        });
        st.append_delta("hel");
        st.append_delta("lo");
        st.append_reasoning("chain of thought");
        let (agent, history, _home, _env) = dummy_agent();
        st.end_turn("hello", "p", "m", true, false, 10, 5, Some(8), None, &agent);
        // user · reply · reasoning — the canonical text replaced the
        // streamed one IN PLACE (the trailing reasoning had closed it),
        // and the reasoning SURVIVES the turn, collapsed, instead of
        // being deleted (USER decision).
        assert_eq!(st.transcript.blocks.len(), 3);
        assert_eq!(st.transcript.blocks[1].body, "hello");
        let r = &st.transcript.blocks[2];
        assert_eq!(r.kind, Kind::Reasoning);
        assert!(r.collapsed, "final reasoning is kept, collapsed");
        assert_eq!(r.body, "chain of thought");
        assert_eq!(st.prompt_tok, 10);
        assert_eq!(st.completion_tok, 5);
        assert_eq!(st.cached_tok, 8, "reported cache accumulates");
        assert!(st.cache_seen);
        assert!(!st.busy);
        drop(history);
    }

    /// `/steer` typed DURING a turn must land in this turn, promptly.
    ///
    /// The reported bug: "steers hang the entire frontend". They did.
    /// `spawn_turn` held the agent's `Mutex` guard for the whole
    /// `run_turn(...).await`, and `send_current`'s steer arm asked for
    /// that same guard — on the single thread that also owns
    /// `terminal.draw()` and `ui_rx.recv()`. So pressing Enter on
    /// `/steer` froze the terminal until the turn finished on its own,
    /// at which point the steer was queued for the NEXT turn instead.
    ///
    /// This drives a REAL turn against a provider that opens a stream
    /// and stalls, so the turn is genuinely in flight while the steer is
    /// typed. It is timed on purpose: correctness here IS promptness.
    #[test]
    fn steer_lands_mid_turn_without_blocking_the_ui_thread() {
        use std::time::{Duration, Instant};
        let (url, release) = veles_core::testenv::stalling_provider();
        let (agent, history, _home, _env) = dummy_agent_at(&url);
        let (ui_tx, _ui_rx) = mpsc::channel::<UiEvent>();

        let cancel = veles_core::providers::CancelToken::new();
        spawn_turn(
            agent.clone(),
            history.clone(),
            "start something long".into(),
            ui_tx.clone(),
            cancel.clone(),
        );
        // Let the turn reach the socket and stall there.
        std::thread::sleep(Duration::from_millis(400));

        let mut st = UiState::new("s1".into());
        st.busy = true;
        st.input = "/steer use Python instead".into();
        st.caret = st.input.len();
        st.agent_snapshot = Some(agent.clone());

        let started = Instant::now();
        send_current(&mut st, &agent, &history, &ui_tx);
        let took = started.elapsed();

        // The steer reached the queue the turn actually reads.
        assert_eq!(
            cur(&agent).pending_steer.lock_ok().as_deref(),
            Some("use Python instead"),
            "the steer must reach THIS turn's queue"
        );
        // …and the UI thread was never parked behind the turn. The old
        // code blocked here for the turn's whole duration; the stall is
        // 20s, so anything near that is the bug returning.
        assert!(
            took < Duration::from_secs(2),
            "send_current blocked the UI thread for {took:?} while a turn \
             was in flight — that is the freeze this test exists for"
        );
        assert!(st.input.is_empty(), "the steer consumed the composer");

        let _ = release.send(());
        cancel.cancel();
    }

    #[test]
    fn unreported_cache_stays_unknown() {
        let mut st = UiState::new("s1".into());
        let (agent, history, _home, _env) = dummy_agent();
        st.end_turn("hi", "p", "m", true, false, 10, 5, None, None, &agent);
        assert!(!st.cache_seen, "no cache field means unknown, not zero");
        assert_eq!(st.cached_tok, 0);
        drop(history);
    }

    /// R3: the same turn, streamed live and rebuilt from the store,
    /// must RENDER byte-identically — one `render_msg`, one layout.
    /// This is the pin for "live ≡ replay"; layout equality of the
    /// block model alone (transcript.rs) is necessary but not
    /// sufficient, because the renderer is where the old TUI drifted.
    #[test]
    fn live_and_reload_render_byte_identically() {
        use veles_core::transcript::Transcript as Tr;
        let th = veles_core::theme::synthwave();

        // Live: a two-round turn with a reasoning segment.
        let mut live = Tr::new();
        live.push(TEvent::User {
            text: "do the thing".into(),
            time: "12:00".into(),
            injection: None,
        });
        live.push(TEvent::ReasoningDelta("first I check".into()));
        live.push(TEvent::Delta("let me look".into()));
        live.push(TEvent::ToolStart {
            name: "read_file".into(),
            args: "{\"path\":\"a\"}".into(),
            call_id: "c1".into(),
        });
        live.push(TEvent::ToolEnd {
            name: "read_file".into(),
            result: "contents".into(),
            call_id: "c1".into(),
        });
        live.push(TEvent::Delta("here is the answer".into()));
        live.finish_reply("here is the answer");

        // The store's version of the same turn.
        let msgs = vec![
            veles_core::providers::ChatMessage {
                role: "user".into(),
                content: "do the thing".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
            veles_core::providers::ChatMessage {
                role: "assistant".into(),
                content: "let me look".into(),
                reasoning: Some("first I check".into()),
                display_reasoning: None,
                tool_calls: Some(serde_json::json!([{
                    "id": "c1",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"}
                }])),
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
            veles_core::providers::ChatMessage {
                role: "tool".into(),
                content: "contents".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: Some(veles_core::providers::ToolMeta {
                    tool_call_id: "c1".into(),
                    name: "read_file".into(),
                }),
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
            veles_core::providers::ChatMessage {
                role: "assistant".into(),
                content: "here is the answer".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
        ];
        let ids = [1, 2, 3, 4];
        let times = ["12:00", "12:00", "12:01", "12:01"];
        let replay = veles_core::transcript::from_history(
            &msgs,
            &ids,
            &times.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            &[],
            &|_| None,
        );

        let flatten = |t: &Tr| -> Vec<String> {
            t.blocks
                .iter()
                .map(|b| {
                    render_msg(b, &th, 60, false)
                        .lines
                        .iter()
                        .map(|l| {
                            l.spans
                                .iter()
                                .map(|s| s.content.to_string())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                // The live path stamps blocks with the wall clock as
                // they open; replay reads the stored stamp. Identical
                // LAYOUT is the guarantee — normalise the `hh:mm:ss `
                // (live) and `hh:mm ` (these fixture rows) prefixes so
                // the assertion says exactly that.
                .map(|s| {
                    let b = s.as_bytes();
                    let two =
                        |i: usize| b.len() > i + 1 && b[i..i + 2].iter().all(u8::is_ascii_digit);
                    if two(0) && b.get(2) == Some(&b':') && two(3) {
                        if b.get(5) == Some(&b' ') {
                            return s[6..].to_string();
                        }
                        if b.get(5) == Some(&b':') && two(6) && b.get(8) == Some(&b' ') {
                            return s[9..].to_string();
                        }
                    }
                    s
                })
                .collect()
        };
        assert_eq!(
            flatten(&live),
            flatten(&replay),
            "the live stream and the reloaded rows must render identically"
        );
    }

    /// The message-origin family: an injected row classed
    /// `origin:<surface>` paints with its surface named in the head —
    /// "⇒ origin: tui ▸" — via the SAME split that keeps every other
    /// injection class on "⇒ <class> ▸". A bare `⇒ origin: … ▸` line
    /// is the difference between "which surface did this come from"
    /// being answerable at a glance, and being the reason this row was
    /// built.
    #[test]
    fn the_origin_injection_names_its_surface_in_the_head() {
        use veles_core::transcript::{Block, Injection};
        let th = veles_core::theme::synthwave();
        let block = Block {
            kind: Kind::User,
            time: "09:41".into(),
            body: "[origin] this message arrived via the tui surface".into(),
            open: false,
            collapsed: false,
            tool: None,
            note: None,
            injection: Some(Injection {
                class: "origin:tui".into(),
                toggle: None,
            }),
        };
        let r = render_msg(&block, &th, 60, false);
        let head: String = r.lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            head.starts_with("⇒ origin: tui ▸"),
            "the origin row's head names its surface: {head:?}"
        );
        // And a non-origin class still reads through the generic path.
        let steer = Block {
            injection: Some(Injection {
                class: "steer".into(),
                toggle: None,
            }),
            ..block
        };
        let rs = render_msg(&steer, &th, 60, false);
        let shead: String = rs.lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(
            shead.starts_with("⇒ steer ▸"),
            "other classes keep the generic head: {shead:?}"
        );
    }

    #[test]
    fn tool_entries_survive_end_turn() {
        let mut st = UiState::new("s1".into());
        st.emit(TEvent::User {
            text: "go".into(),
            time: "12:00".into(),
            injection: None,
        });
        st.append_delta("working…");
        st.emit(TEvent::ToolStart {
            name: "read_file".into(),
            args: "{\"path\":\"a\"}".into(),
            call_id: String::new(),
        });
        let (agent, history, _home, _env) = dummy_agent();
        st.end_turn("done", "p", "m", true, false, 1, 1, None, None, &agent);
        // THE REPORTED BUG, from the other side: the tool used to sit
        // after both replies because round two welded onto round one.
        // Now: user · reply · tool · reply.
        let kinds: Vec<Kind> = st.transcript.blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::User, Kind::Reply, Kind::Tool, Kind::Reply]
        );
        assert_eq!(st.transcript.blocks[1].body, "working…");
        assert_eq!(st.transcript.blocks[3].body, "done");
        assert_eq!(
            st.transcript.blocks[2]
                .tool
                .as_ref()
                .map(|t| t.name.as_str()),
            Some("read_file")
        );
        drop(history);
    }

    /// Agent + history, plus the scratch home and env guard that must
    /// outlive the caller (dropping them mid-test would release the
    /// lock and delete the VELES_HOME the Agent is using).
    type DummyAgent = (
        AgentHandle,
        Arc<Mutex<Vec<veles_core::providers::ChatMessage>>>,
        tempfile::TempDir,
        veles_core::testenv::EnvGuard,
    );

    fn dummy_agent() -> DummyAgent {
        dummy_agent_at("http://127.0.0.1:0")
    }

    fn dummy_agent_at(base_url: &str) -> DummyAgent {
        // Agent::new bootstraps the memory hierarchy from the AMBIENT
        // VELES_HOME. Without a private one this wrote into whichever
        // scratch dir another test had set.
        let (home, env) = veles_core::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            veles_core::store::Store::open(&dir.path().join("t.db")).unwrap(),
        ));
        // A config that can build routes: provider + default model.
        let mut cfg = veles_core::config::VelesConfig::default();
        cfg.model.provider = Some("test".into());
        cfg.model.default = Some("test-model".into());
        cfg.custom_providers = vec![veles_core::config::ProviderEntry {
            name: "test".into(),
            base_url: base_url.into(),
            key_env: Some("VELES_TEST_KEY".into()),
            api_mode: "chat_completions".into(),
            driver: None,
            model: None,
            models: Default::default(),
            context_length: None,
            reasoning_effort: None,
            discover: None,
            extra_body: None,
            cache_mode: None,
            replay_reasoning: None,
            built_in_tools: vec![],
            endpoint_path: None,
            auth_style: None,
            api_version: None,
            max_output_tokens: None,
        }];
        let cfg = Arc::new(cfg);
        store.lock().unwrap().create_session("s1").unwrap();
        let registry = Arc::new(veles_core::tools::builtin_registry(
            store.clone(),
            None,
            "s1",
        ));
        let agent = Agent::new(
            cfg,
            veles_core::hooks::HookBus::new(None),
            registry,
            store,
            "s1".into(),
        )
        .unwrap();
        // home + env are RETURNED so they outlive the caller: dropping
        // the guard here would release the lock (and delete the scratch
        // VELES_HOME) while the test is still running.
        (handle(agent), Arc::new(Mutex::new(Vec::new())), home, env)
    }

    /// Like `dummy_agent_at`, but with model aliases so the picker has
    /// something to select from (current + aliases) and `set_model` has
    /// an unambiguous alias to resolve.
    fn dummy_agent_with_aliases(
        aliases: std::collections::HashMap<String, veles_core::config::ModelAlias>,
    ) -> DummyAgent {
        let (home, env) = veles_core::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Mutex::new(
            veles_core::store::Store::open(&dir.path().join("t.db")).unwrap(),
        ));
        let mut cfg = veles_core::config::VelesConfig::default();
        cfg.model.provider = Some("test".into());
        cfg.model.default = Some("test-model".into());
        cfg.model_aliases = aliases;
        cfg.custom_providers = vec![veles_core::config::ProviderEntry {
            name: "test".into(),
            base_url: "http://127.0.0.1:0".into(),
            key_env: Some("VELES_TEST_KEY".into()),
            api_mode: "chat_completions".into(),
            driver: None,
            model: Some("test-model".into()),
            models: Default::default(),
            context_length: None,
            reasoning_effort: None,
            discover: None,
            extra_body: None,
            cache_mode: None,
            replay_reasoning: None,
            built_in_tools: vec![],
            endpoint_path: None,
            auth_style: None,
            api_version: None,
            max_output_tokens: None,
        }];
        let cfg = Arc::new(cfg);
        store.lock().unwrap().create_session("s1").unwrap();
        let registry = Arc::new(veles_core::tools::builtin_registry(
            store.clone(),
            None,
            "s1",
        ));
        let agent = Agent::new(
            cfg,
            veles_core::hooks::HookBus::new(None),
            registry,
            store,
            "s1".into(),
        )
        .unwrap();
        (handle(agent), Arc::new(Mutex::new(Vec::new())), home, env)
    }

    // ── R4.8: key chords, composer undo, and the command palette ────

    /// Ctrl-/ (and Ctrl-_) undo the last edit, and a run of adjacent
    /// characters is ONE edit — undoing after typing a word gives back
    /// the pre-word state, not the pre-letter state.
    #[test]
    fn ctrl_slash_undo_reverts_the_last_edit_as_a_run() {
        let mut st = UiState::new("s1".into());
        composer_insert(&mut st, "h");
        composer_insert(&mut st, "ell");
        composer_insert(&mut st, "o");
        assert_eq!(st.input, "hello");
        assert_eq!(st.undo.len(), 1, "an unbroken char run is one undo step");

        composer_undo(&mut st);
        assert_eq!(st.input, "", "undo reverts the whole run");

        // A new edit snapshots afresh; a backspace is its own step.
        composer_insert(&mut st, "wo");
        composer_insert(&mut st, "rld");
        assert_eq!(st.input, "world");
        backspace(&mut st);
        assert_eq!(st.input, "worl");
        composer_undo(&mut st);
        assert_eq!(st.input, "world", "undo restores the backspaced char");
        composer_undo(&mut st);
        assert_eq!(st.input, "", "and again reverses the run");
    }

    /// The reported bug: in all three keymaps the `KeyCode::Char(c)`
    /// catch-alls typed the letter for ANY unbound chord — so Ctrl-Z
    /// typed `z`, Ctrl-A typed `a`, Alt-b typed `b`. Now only bare,
    /// unmodified characters enter the composer; chords are reserved.
    #[test]
    fn unbound_chords_stop_typing_into_the_composer_in_every_keymap() {
        let (agent, history, _home, _env) = dummy_agent();
        let (tx, _rx) = mpsc::channel::<UiEvent>();
        for keymap in [
            veles_core::keymap::Keymap::Normie,
            veles_core::keymap::Keymap::Emacs,
            veles_core::keymap::Keymap::Vi,
        ] {
            let mut st = UiState::new("s1".into());
            st.keymap = keymap;
            st.vi_insert = true;
            let chord = |st: &mut UiState, code: KeyCode, mods: KeyModifiers| {
                handle_key(st, &agent, &history, &tx, event::KeyEvent::new(code, mods));
            };
            // `q` is unbound in every keymap: Ctrl-Q and Alt-q must not
            // insert, and the same guard is what keeps Ctrl-Z from being
            // able to type `z` even before its suspend binding is seen.
            chord(&mut st, KeyCode::Char('q'), KeyModifiers::CONTROL);
            chord(&mut st, KeyCode::Char('q'), KeyModifiers::ALT);
            assert!(
                st.input.is_empty(),
                "Ctrl-Q and Alt-q must not type into {keymap:?}"
            );
            chord(&mut st, KeyCode::Char('x'), KeyModifiers::empty());
            assert_eq!(st.input, "x", "a bare character still inserts");
        }
    }

    /// The render fence: ANSI sequences vanish, stray control bytes
    /// become a VISIBLE '·', structure (\t) and plain text survive.
    /// One `opencode serve` log wall must not repaint the screen.
    #[test]
    fn scrub_controls_neutralizes_terminal_junk() {
        assert_eq!(scrub_controls("clean text"), "clean text");
        assert_eq!(scrub_controls("tab\there"), "tab\there");
        // CSI: colors, cursor moves, erase-line.
        assert_eq!(scrub_controls("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(scrub_controls("a\x1b[2Kb"), "ab");
        // OSC 52 (clipboard) with BEL, OSC title with ST.
        assert_eq!(scrub_controls("\x1b]52;c;aGVsbG8=\x07"), "");
        assert_eq!(scrub_controls("\x1b]0;title\x1b\\kept"), "kept");
        // Two-char escapes.
        assert_eq!(scrub_controls("x\x1bMy"), "xy");
        // Other C0 controls map to a visible dot.
        assert_eq!(scrub_controls("a\rb\x00c\x07d"), "a·b·c·d");
        // A sequence cut off mid-flight (truncated capture) drops.
        assert_eq!(scrub_controls("ok\x1b[31"), "ok");
    }

    /// Multiline geometry: byte caret → (line, column-in-line), with
    /// line-boundary carets belonging to the line they END.
    #[test]
    fn caret_line_col_maps_bytes_to_lines() {
        let s = "ab\ncdé\nf";
        assert_eq!(caret_line_col(s, 0), (0, 0));
        assert_eq!(caret_line_col(s, 2), (0, 2)); // on the \n
        assert_eq!(caret_line_col(s, 3), (1, 0));
        assert_eq!(caret_line_col(s, 7), (1, 4)); // past é (2 bytes)
        assert_eq!(caret_line_col(s, 9), (2, 1)); // end
                                                  // Trailing newline: caret sits on the empty last line.
        assert_eq!(caret_line_col("x\n", 2), (1, 0));
        // Out-of-range clamps.
        assert_eq!(caret_line_col("ab", 99), (0, 2));
        assert_eq!(line_span("ab\ncdé\nf", 1), (3, 7));
        assert_eq!(line_span("x\n", 1), (2, 2));
    }

    /// The composer grows with its content, capped at half the
    /// terminal — the transcript never collapses.
    #[test]
    fn composer_height_grows_then_caps_at_half_the_terminal() {
        assert_eq!(composer_height(1, 40), 3);
        assert_eq!(composer_height(5, 40), 7);
        assert_eq!(composer_height(500, 40), 20);
        assert_eq!(composer_height(500, 10), 5);
        // Tiny terminals still get the classic single-line box.
        assert_eq!(composer_height(1, 4), 3);
    }

    /// Scroll-following: the caret row is always on screen, and the
    /// scroll only moves as far as it must.
    #[test]
    fn ensure_caret_row_visible_follows_the_caret() {
        assert_eq!(ensure_caret_row_visible(0, 2, 3), 0); // inside
        assert_eq!(ensure_caret_row_visible(0, 0, 3), 0);
        assert_eq!(ensure_caret_row_visible(0, 5, 3), 3); // below: 5+1-3
        assert_eq!(ensure_caret_row_visible(0, 7, 3), 5);
        assert_eq!(ensure_caret_row_visible(5, 2, 3), 2); // above
        assert_eq!(ensure_caret_row_visible(2, 4, 3), 2); // 4 < 2+3: stays
    }

    /// Horizontal scroll: 0 while the caret fits, else the caret parks
    /// one cell from the right edge.
    #[test]
    fn hscroll_offset_keeps_the_caret_on_screen() {
        assert_eq!(hscroll_offset(5, 20), 0);
        assert_eq!(hscroll_offset(19, 20), 0);
        assert_eq!(hscroll_offset(20, 20), 1);
        assert_eq!(hscroll_offset(25, 20), 6);
    }

    /// skip_width drops leading DISPLAY columns, char-safe for wide
    /// glyphs (the caret row's horizontal scroll).
    #[test]
    fn skip_width_counts_display_columns() {
        assert_eq!(skip_width("héllo", 0), "héllo");
        assert_eq!(skip_width("héllo", 1), "éllo");
        assert_eq!(skip_width("日本", 2), "本");
        assert_eq!(skip_width("日本", 4), "");
        assert_eq!(skip_width("ab", 5), "");
    }

    /// ↑/↓ walk the lines of a multiline input first and only fall
    /// through to history at the edges — the readline contract.
    #[test]
    fn up_down_walk_lines_then_history() {
        let mut st = UiState::new("s1".into());
        st.input = "one\ntwo\nthree".into();
        st.hist = vec!["older".into()];
        st.caret = st.input.len(); // end of "three"
        up_from(&mut st);
        assert_eq!(caret_line_col(&st.input, st.caret), (1, 3), "col clamped");
        up_from(&mut st);
        assert_eq!(caret_line_col(&st.input, st.caret), (0, 3));
        up_from(&mut st);
        assert_eq!(st.input, "older", "off the top line = history");
        // Single-line history entry: ↓ at its end is history-next,
        // which resets to the empty composer.
        down_from(&mut st);
        assert!(st.input.is_empty());
        assert_eq!(st.caret, 0);
    }

    /// Home/End are line-scoped in a multiline composer (and identical
    /// to the old whole-input behavior on one line).
    #[test]
    fn home_end_are_line_scoped() {
        let mut st = UiState::new("s1".into());
        st.input = "abc\ndef".into();
        st.caret = 7;
        home(&mut st);
        assert_eq!(st.caret, 4);
        end(&mut st);
        assert_eq!(st.caret, 7);
        st.caret = 2;
        home(&mut st);
        assert_eq!(st.caret, 0);
        // Single-line: the classic semantics.
        st.input = "abc".into();
        st.caret = 2;
        home(&mut st);
        assert_eq!(st.caret, 0);
        end(&mut st);
        assert_eq!(st.caret, 3);
    }

    /// Ctrl/Alt/Shift-Enter insert a newline in EVERY keymap; bare
    /// Enter still submits (the composer empties).
    #[test]
    fn newline_bindings_are_the_same_contract_in_every_keymap() {
        let (agent, history, _home, _env) = dummy_agent();
        let (tx, _rx) = mpsc::channel::<UiEvent>();
        for keymap in [
            veles_core::keymap::Keymap::Normie,
            veles_core::keymap::Keymap::Emacs,
            veles_core::keymap::Keymap::Vi,
        ] {
            for mods in [
                KeyModifiers::CONTROL,
                KeyModifiers::ALT,
                KeyModifiers::SHIFT,
            ] {
                let mut st = UiState::new("s1".into());
                st.keymap = keymap;
                st.vi_insert = true;
                handle_key(
                    &mut st,
                    &agent,
                    &history,
                    &tx,
                    event::KeyEvent::new(KeyCode::Enter, mods),
                );
                assert_eq!(
                    st.input, "\n",
                    "{mods:?}-Enter inserts a newline in {keymap:?}"
                );
            }
            // Bare Enter submits: input is consumed, not edited.
            let mut st = UiState::new("s1".into());
            st.keymap = keymap;
            st.vi_insert = true;
            st.input = "hello".into();
            st.caret = 5;
            handle_key(
                &mut st,
                &agent,
                &history,
                &tx,
                event::KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            );
            assert!(st.input.is_empty(), "Enter submits in {keymap:?}");
        }
    }

    /// Clicking a row of the (possibly scrolled) composer lands the
    /// caret on THAT line, at the clicked display column — wide glyphs
    /// included.
    #[test]
    fn clicking_a_composer_row_lands_the_caret_on_that_line() {
        let (agent, history, _home, _env) = dummy_agent();
        let (tx, _rx) = mpsc::channel::<UiEvent>();
        let mut st = UiState::new("s1".into());
        st.input = "first\n日本語\nthird".into();
        st.input_area = Some(Rect {
            x: 0,
            y: 10,
            width: 40,
            height: 5,
        });
        let ev = event::MouseEvent {
            kind: event::MouseEventKind::Down(event::MouseButton::Left),
            column: 5, // border + "❯ " = 3 cells of chrome → col 2 of the line
            row: 11,   // first text row
            modifiers: KeyModifiers::empty(),
        };
        handle_mouse(&mut st, &agent, &history, &tx, ev);
        assert_eq!(st.caret, 2, "row 1 click → caret after 'fi'");
        // A click on a scrolled-off view addresses the RIGHT line:
        // pretend we scrolled one line down.
        st.input_vscroll = 1;
        handle_mouse(&mut st, &agent, &history, &tx, ev);
        // want=2 columns into 日本語: 日 is 2 cells wide, so the caret
        // lands just past it (byte 3 of the line).
        assert_eq!(st.caret, 6 + 3, "scrolled view: line '日本語', past 日");
    }

    /// Ctrl-K (and Alt-x) open the palette; selecting its `quit` entry
    /// must really quit, not be the dead entry it was ("not reachable
    /// from the loop").
    #[test]
    fn the_palette_quit_entry_really_quits() {
        let (agent, history, _home, _env) = dummy_agent();
        let (tx, _rx) = mpsc::channel::<UiEvent>();
        let mut st = UiState::new("s1".into());
        let ctrl_k = event::KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
        handle_key(&mut st, &agent, &history, &tx, ctrl_k);
        assert!(st.palette.is_some(), "Ctrl-K opens the palette");

        run_palette_action(&mut st, &agent, &history, &tx, PaletteAction::Quit);
        assert!(st.quit, "palette quit sets the quit flag");
    }

    /// The palette's command rows come from the ONE help registry — a
    /// command dropped from `help_entries` falls out of the palette too,
    /// which is the mutation that must turn this test red.
    #[test]
    fn the_palette_mirrors_the_help_registry() {
        let st = UiState::new("s1".into());
        let commands: Vec<(String, String)> = palette_entries(&st)
            .into_iter()
            .filter_map(|e| match e.action {
                PaletteAction::Command(h) => Some((h, e.desc)),
                _ => None,
            })
            .collect();
        for (label, desc) in crate::commands::help_entries(crate::commands::Surface::Tui) {
            let head = label.split_whitespace().next().unwrap_or(label);
            let row = commands
                .iter()
                .find(|(h, _)| h == head)
                .unwrap_or_else(|| panic!("help row '{label}' must appear as a palette command"));
            // AND ITS DESCRIPTION. The registry's second column used to
            // be dropped on the way in, so the palette rendered a column
            // of bare names — the one screen where you look for a
            // command you cannot name showed you only names.
            assert_eq!(
                row.1, desc,
                "help row '{label}' must carry its description into the palette"
            );
        }
    }

    /// The engine-state commands the plan called out / R4.8 forces are
    /// reachable from the palette without typing their arg — the picker
    /// is the C3 "editors, not dumps" half made visible.
    #[test]
    fn the_palette_offers_the_engine_state_commands() {
        let st = UiState::new("s1".into());
        let heads: Vec<String> = palette_entries(&st)
            .into_iter()
            .filter_map(|e| match e.action {
                PaletteAction::Command(h) => Some(h),
                _ => None,
            })
            .collect();
        for want in [
            "/model",
            "/memory",
            "/skills",
            "/plugins",
            "/acl",
            "/checkpoints",
            "/hooks",
            "/context",
            "/todos",
            "/queue",
            "/compress",
        ] {
            assert!(heads.iter().any(|h| h == want), "palette must offer {want}");
        }
    }

    /// "Session jump": every session is a palette row, selectable to
    /// switch — type a name to narrow to it.
    #[test]
    fn the_palette_lists_sessions_for_jumping() {
        let mut st = UiState::new("s1".into());
        st.sessions = vec![
            SessionItem {
                id: "s1".into(),
                label: "today".into(),
                count: 3,
            },
            SessionItem {
                id: "s2".into(),
                label: "older".into(),
                count: 1,
            },
        ];
        let entries = palette_entries(&st);
        assert!(entries.iter().any(|e| matches!(
            &e.action,
            PaletteAction::SwitchTo(id) if id == "s2"
        ) && e.label == "session: older"));
        // A session row explains itself with its size: two sessions can
        // share a title, and then the count is the only thing that tells
        // them apart before you jump.
        let older = entries
            .iter()
            .find(|e| e.label == "session: older")
            .expect("the row exists");
        assert!(
            older.desc.contains('1'),
            "the session row carries its message count: {:?}",
            older.desc
        );
    }

    /// The description is SEARCHABLE. Finding a command you cannot name
    /// is the whole reason the column exists, and matching only labels
    /// would leave the prose decorative — "background shell" must reach
    /// `/background`, whose name is only half of that.
    #[test]
    fn the_palette_is_searchable_by_description() {
        let mut st = UiState::new("s1".into());
        st.palette = Some(PaletteState {
            query: "background shell".into(),
            sel: 0,
        });
        let hits = filtered_palette(&st);
        assert!(
            hits.iter()
                .any(|e| matches!(&e.action, PaletteAction::Command(h) if h == "/background")),
            "a description match must find /background: {:?}",
            hits.iter().map(|e| &e.label).collect::<Vec<_>>()
        );

        // A label hit WINS over a description hit: `/todos` is what the
        // user meant by "todos", and it is the head of the list rather
        // than merely somewhere in it.
        let mut st = UiState::new("s1".into());
        st.palette = Some(PaletteState {
            query: "todos".into(),
            sel: 0,
        });
        assert_eq!(
            filtered_palette(&st).first().map(|e| e.label.as_str()),
            Some("/todos"),
            "an exact label match outranks description matches"
        );
    }

    /// The palette DRAWS the description. `draw_palette` is where the
    /// reader meets the column — a test that stops at
    /// `palette_entries` would pass on a popup that renders labels and
    /// drops the second half of every row on the floor.
    #[test]
    fn the_palette_renders_the_description_column() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut st = UiState::new("s1".into());
        st.palette = Some(PaletteState {
            query: "/mech".into(),
            sel: 0,
        });
        let mut term = Terminal::new(TestBackend::new(120, 40)).expect("test terminal");
        term.draw(|f| draw_palette(f, &st, &veles_core::theme::synthwave()))
            .expect("draw");
        let buf = term.backend().buffer().clone();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf.get(x, y).symbol());
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("/mech"),
            "the command is on screen:\n{screen}"
        );
        // The registry's own words for /mech, reached from the rendered
        // buffer so the assertion cannot pass on a hardcoded copy.
        let desc = crate::commands::help_entries(crate::commands::Surface::Tui)
            .into_iter()
            .find(|(l, _)| l.starts_with("/mech"))
            .map(|(_, d)| d)
            .expect("/mech is in the help registry");
        let head: String = desc.chars().take(24).collect();
        assert!(
            screen.contains(&head),
            "the description must be rendered beside the name ({head:?}):\n{screen}"
        );
    }

    // ── R4.8c: the /model picker ("editors, not dumps") ───────────────

    /// Bare `/model` opens a picker, not a markdown report dump. This is
    /// the whole point of "editors, not dumps": a command whose result
    /// is a choice gets an interactive selector. Mutation that reverts
    /// it: dropping the intercept makes `/model` answer the shared
    /// report and leaves `st.picker == None`.
    #[test]
    fn bare_model_opens_a_picker_not_a_dump() {
        let (agent, history, _home, _env) = dummy_agent();
        let mut st = UiState::new("s1".into());
        let out = local_command(&mut st, &agent, &history, "/model");
        assert!(
            out.is_some() && st.picker.is_some(),
            "bare /model must open the picker"
        );
        let picker = st.picker.as_ref().unwrap();
        assert_eq!(picker.title, "model");
        // The current (default) model is surfaced first and marked.
        assert!(
            picker
                .rows
                .iter()
                .any(|r| r.label == "test-model (current)"),
            "the picker marks the current model"
        );
    }

    /// Selecting a picker row runs its value through the same `set_model`
    /// as `/model <name>` — so an alias resolves provider + model in one
    /// place (the picker cannot disagree with the command). Mutation
    /// that reverts it: making `run_picker_selection` a no-op leaves
    /// `overrides.model` unchanged.
    #[test]
    fn selecting_a_model_runs_it_through_set_model() {
        let mut aliases = std::collections::HashMap::new();
        aliases.insert(
            "fast".to_string(),
            veles_core::config::ModelAlias {
                provider: "test".to_string(),
                model: "test-fast".to_string(),
                base_url: String::new(),
                reasoning: None,
            },
        );
        let (agent, history, _home, _env) = dummy_agent_with_aliases(aliases);
        let mut st = UiState::new("s1".into());
        st.picker = Some(model_picker(&agent));
        // current ("test-model") is selection 0, "fast" is selection 1.
        let fast_sel = st
            .picker
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|r| r.value == "fast")
            .expect("the alias is an option");
        assert_ne!(
            fast_sel,
            st.picker.as_ref().unwrap().sel,
            "current model is the default selection, not the alias"
        );

        run_picker_selection(&mut st, &agent, &history, fast_sel);
        assert_eq!(
            cur(&agent).overrides.read_ok().model.as_deref(),
            Some("test-fast"),
            "Enter on the alias resolves through set_model"
        );
    }

    // ── R4.8c: the collection panes ("editors, not dumps") ────────────

    /// Every collection command whose answer is a choice now opens a
    /// modal picker (the "editors, not dumps" headline generalised from
    /// /model) instead of dumping prose into the transcript. Mutation:
    /// removing the intercept makes the shared report run and leaves
    /// `st.picker == None`.
    #[test]
    fn the_collection_commands_open_pickers_not_reports() {
        let (agent, history, _home, _env) = dummy_agent();
        for (cmd, prefix) in [
            ("/memory", "memory"),
            ("/skills", "skills"),
            ("/plugins", "plugins"),
            ("/hooks", "hooks"),
            ("/acl", "acl"),
            ("/checkpoints", "checkpoints"),
            ("/diff", "review"),
        ] {
            let mut st = UiState::new("s1".into());
            let out = local_command(&mut st, &agent, &history, cmd);
            assert!(out.is_some(), "{cmd} must be handled");
            let picker = st
                .picker
                .as_ref()
                .unwrap_or_else(|| panic!("{cmd} opens a picker"));
            assert!(
                picker.title.starts_with(prefix),
                "{cmd} title should start with '{prefix}': {}",
                picker.title
            );
        }
    }

    /// /memory lists every memory file, not just the two resident
    /// stores — with memory.d gone this picker is how the TUI reaches a
    /// reference file at all — and Enter shows its content.
    #[test]
    fn the_memory_picker_lists_every_file_with_its_content() {
        let (_agent, _history, _home, _env) = dummy_agent();
        let mem = veles_core::memory::MemoryStore::new();
        mem.write(veles_core::memory::StoreKind::Memory, &["a note".into()])
            .unwrap();
        mem.write_file("reference/topic.md", "# topic\nthe body")
            .unwrap();
        mem.write_file("reference/gone.md", "x").unwrap();
        mem.delete_file("reference/gone.md").unwrap();
        let p = memory_picker();
        let row = p
            .rows
            .iter()
            .find(|r| r.value == "reference/topic.md")
            .expect("a reference file has a row");
        assert!(row.detail.as_deref().unwrap_or("").contains("the body"));
        assert!(matches!(row.action, Some(PickerAction::View)));
        assert!(!p.rows.iter().any(|r| r.value == "reference/gone.md"));
        assert_eq!(
            p.rows
                .iter()
                .filter(|r| r.label.contains("system/core.md"))
                .count(),
            1,
            "the resident store is listed once"
        );
    }

    /// The Files browser: directories descend (re-open via `/files`),
    /// files open a preview in the pager. One level at a time, dirs
    /// first — the lazy shape the desktop tree also uses.
    #[test]
    fn the_files_browser_descends_and_previews() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\n").unwrap();
        let p = files_picker(dir.path().to_str().unwrap());
        assert_eq!(p.title, format!("files — {}", dir.path().display()));

        let sub = p.rows.iter().find(|r| r.label == "sub/").expect("dir row");
        assert!(
            matches!(sub.action, Some(PickerAction::Command)),
            "a directory descends, it does not preview"
        );
        assert_eq!(
            sub.value,
            format!("/files {}", dir.path().join("sub").display()),
            "descend re-runs /files at the child path"
        );

        let txt = p
            .rows
            .iter()
            .find(|r| r.label == "a.txt")
            .expect("file row");
        assert!(
            matches!(txt.action, Some(PickerAction::View)),
            "a file opens in the pager"
        );
        assert!(
            txt.detail.as_deref().unwrap_or("").contains("hello"),
            "the pager detail is the file's content"
        );
    }

    /// A checkpoint row IS an undo point: Enter runs it through the
    /// shared `/restore <id>`, so restore's own error handling applies
    /// (the picker cannot disagree with the command). Mutation: dropping
    /// the Command action makes the row inert on Enter.
    #[test]
    fn checkpoint_rows_restore_via_the_shared_command() {
        let (agent, _history, _home, _env) = dummy_agent();
        let id = cur(&agent)
            .store
            .lock_ok()
            .checkpoint_add("s1", "/tmp/edited.rs", None, None, 12, "patch", Some("fix"))
            .expect("seed a checkpoint");
        let p = checkpoints_picker(&agent);
        let row = p
            .rows
            .iter()
            .find(|r| r.value == format!("/restore {id}"))
            .expect("the checkpoint is a restorable row");
        assert!(matches!(row.action, Some(PickerAction::Command)));
        assert!(
            row.label.contains("patch"),
            "label names the tool: {}",
            row.label
        );
    }

    /// A `View` row opens the scrollable pager over its detail text.
    /// Mutation: routing `View` to the status bar instead of `st.detail`
    /// leaves `st.detail == None`.
    #[test]
    fn a_view_row_opens_the_pager() {
        let (agent, history, _home, _env) = dummy_agent();
        let mut st = UiState::new("s1".into());
        st.picker = Some(Picker {
            title: "t".into(),
            rows: vec![PickerRow {
                label: "the skill".into(),
                value: "skill-name".into(),
                detail: Some("line one\nline two".into()),
                action: Some(PickerAction::View),
            }],
            sel: 0,
            kind: PickerAction::View,
        });
        run_picker_selection(&mut st, &agent, &history, 0);
        assert!(st.picker.is_none(), "the picker closes");
        let d = st.detail.as_ref().expect("a View row opens the pager");
        assert_eq!(d.title, "the skill");
        assert!(d.text.contains("line one"));
        assert_eq!(d.scroll, 0, "the pager opens at the top");
    }

    /// The acl picker's promote rows run the SHARED command, so the pane
    /// cannot drift from what `/acl promote` means — the failure mode the
    /// desktop's hand-written command table had. The dry run is a row of
    /// its own and comes first, because a picker that can write
    /// config.toml has to be able to show what it would write. And the
    /// store's USERS are rows beside its grants: a picker that showed only
    /// grants was hiding half of what `--users` folds in.
    #[test]
    fn acl_rows_promote_via_the_shared_command() {
        let (agent, history, _home, _env) = dummy_agent();
        let store = cur(&agent).store.clone();
        veles_core::pairing::grant(&store, "bob!*@*", "trusted", None).unwrap();
        veles_core::pairing::user_save(&store, "nyx", "trusted", &["nyx!~n@h".to_string()])
            .unwrap();

        let p = acl_picker(&agent);
        let labels: Vec<&str> = p.rows.iter().map(|r| r.label.as_str()).collect();
        let preview = p
            .rows
            .iter()
            .position(|r| r.value == "/acl promote --dry-run --users")
            .unwrap_or_else(|| panic!("the dry run is a row of its own: {labels:?}"));
        let write = p
            .rows
            .iter()
            .position(|r| r.value == "/acl promote --users")
            .unwrap_or_else(|| panic!("and so is the write: {labels:?}"));
        assert!(
            matches!(p.rows[preview].action, Some(PickerAction::Command)),
            "Enter runs it rather than paging it"
        );
        assert!(
            matches!(p.rows[write].action, Some(PickerAction::Command)),
            "Enter runs it rather than paging it"
        );
        assert!(preview < write, "the preview comes first");
        assert!(
            labels.iter().any(|l| l.starts_with("grant  bob!*@*")),
            "the store's grant is a row: {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.starts_with("ruser  nyx")),
            "and so is the store's user: {labels:?}"
        );

        // Enter on the preview: the picker closes and the engine's own
        // report lands in the transcript. This agent has no `[acl]` at
        // all, so the honest answer is the refusal — which is the point of
        // asserting on the text rather than on "it did something".
        let mut st = UiState::new("s1".into());
        st.picker = Some(acl_picker(&agent));
        run_picker_selection(&mut st, &agent, &history, preview);
        assert!(st.picker.is_none(), "the picker closes");
        assert!(st.detail.is_none(), "a command row is not a pager row");
        let status = st
            .transcript
            .blocks
            .iter()
            .rev()
            .find(|b| matches!(b.kind, Kind::Status))
            .map(|b| b.body.clone())
            .expect("Enter on a command row reports");
        assert!(
            status.contains("dry run") && status.contains("refusing"),
            "the engine's sentence, not the pane's paraphrase: {status}"
        );
        assert!(
            !veles_core::configedit::config_path().exists(),
            "a dry run on a dormant ACL writes nothing at all"
        );
    }

    /// The reasoning split reaches the footer: a turn whose route reports
    /// reasoning tokens accumulates them (None ≠ zero), so the status bar
    /// can name answer vs thinking. A route that folds thinking into
    /// completion reports nothing and must not read as "0 thinking".
    #[test]
    fn reasoning_tokens_accumulate_without_confusing_none_for_zero() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let agent = crate::handle(veles_core::testenv::mock_agent(
            "http://127.0.0.1:0",
            "tui-split",
        ));
        let mut st = UiState::new("tui-split".to_string());

        // A split route: completion is really answer + thinking.
        st.end_turn(
            "hi",
            "",
            "",
            true,
            false,
            100,
            5100,
            None,
            Some(2800),
            &agent,
        );
        assert_eq!(st.completion_tok, 5100);
        assert_eq!(st.reasoning_tok, Some(2800), "reported reasoning is kept");

        // A non-split route reports None and must leave the accumulator
        // untouched — None is "not reported", never zero.
        st.end_turn("again", "", "", true, false, 100, 900, None, None, &agent);
        assert_eq!(st.reasoning_tok, Some(2800), "None must not zero the total");

        // A second split turn ADDS to the total.
        st.end_turn(
            "third",
            "",
            "",
            true,
            false,
            100,
            200,
            None,
            Some(200),
            &agent,
        );
        assert_eq!(st.reasoning_tok, Some(3000));
    }

    /// THE UPSTREAM PLAN ON THE TUI'S STATUS BAR, NAMED.
    ///
    /// Backport discipline: the desktop and every text surface show the
    /// plan field, and the TUI showed none at all. It now shows it with
    /// the pool's own label — USER 2026-09-20: "the existing usage is
    /// confusing, it conflates the personal and team alibaba tokens …
    /// clearly label the plans separately in the usage widget and the
    /// status line."
    #[test]
    fn the_status_bar_names_the_plan_the_session_draws_from() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let mut cfg = veles_core::config::VelesConfig::default();
        cfg.tokenplan.enabled = true;
        veles_core::tokenplan::invalidate();
        veles_core::tokenplan::seed_cache_for_test(veles_core::tokenplan::QuotaSnapshot {
            fetched_at: chrono::Utc::now(),
            team: None,
            team_error: None,
            personal: Some(veles_core::tokenplan::PersonalQuota {
                windows: vec![veles_core::tokenplan::Window {
                    label: "weekly".into(),
                    used_fraction: Some(0.59),
                    resets_at: None,
                }],
                free_tier: vec![],
            }),
            personal_error: None,
            zai: None,
            zai_error: None,
            kimi: some_kimi_at(1.0),
            kimi_error: None,
        });

        // A personal session reads the PERSONAL figure and says so.
        let p = plan_reading_for(&cfg, "ali-token-plan-personal").expect("a reading");
        assert_eq!(p.0, "personal");
        assert!((p.1 - 0.59).abs() < 1e-9, "the personal weekly: {p:?}");

        // A kimi session reads KIMI's — a different pool under a
        // different vendor, so showing personal's number here is the
        // exact conflation.
        let k = plan_reading_for(&cfg, "custom:kimi-coding-plan").expect("a reading");
        assert_eq!(k.0, "kimi");
        assert!((k.1 - 1.0).abs() < 1e-9, "kimi's own window: {k:?}");

        // A provider on NO plan gets no field — never a fallback to
        // somebody else's pool with a confident number on it.
        assert!(
            plan_reading_for(&cfg, "openrouter").is_none(),
            "no plan means no figure, not another bill's"
        );

        // …and with the tokenplan off there is nothing to read.
        let mut off = veles_core::config::VelesConfig::default();
        off.tokenplan.enabled = false;
        assert!(plan_reading_for(&off, "ali-token-plan-personal").is_none());
        veles_core::tokenplan::invalidate();
    }

    fn some_kimi_at(five_hour: f64) -> Option<veles_core::tokenplan::KimiQuota> {
        Some(veles_core::tokenplan::KimiQuota {
            windows: vec![veles_core::tokenplan::Window {
                label: "5-hour".into(),
                used_fraction: Some(five_hour),
                resets_at: None,
            }],
        })
    }

    /// The label rides with the figure, so the RENDERED bar cannot show
    /// one without the other. This is the user-visible half of the pin
    /// above: a status line that reads "59%" with no plan named is the
    /// conflation, whatever the data underneath says.
    #[test]
    fn the_rendered_status_line_pairs_each_percentage_with_its_plan_name() {
        let mut st = UiState::new("s1".into());
        st.plan_reading = Some(("team".into(), 0.5534));
        let text = status_line_text(&st);
        assert!(text.contains("team 55%"), "{text}");
        assert!(
            !text.contains("plan 55%"),
            "and never a bare 'plan N%', which names nothing: {text}"
        );

        // A different pool, a different name — the two are not
        // interchangeable, which is the whole complaint.
        st.plan_reading = Some(("kimi".into(), 0.10));
        let text = status_line_text(&st);
        assert!(text.contains("kimi 10%"), "{text}");
        assert!(!text.contains("team"), "{text}");

        // No reading: no field at all, not a zero and not a dash that
        // reads like one.
        st.plan_reading = None;
        let text = status_line_text(&st);
        assert!(
            !text.contains('%'),
            "nothing to say, so say nothing: {text}"
        );
    }

    /// Render the status line into a string, the way the widget does.
    ///
    /// A `TestBackend` rather than reading `st` directly, so this pins
    /// what is actually PAINTED — the field's placement, its colour
    /// thresholds and the truncation all live in the draw, and asserting
    /// on the state struct would pass with the draw deleted.
    fn status_line_text(st: &UiState) -> String {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(200, 1)).unwrap();
        term.draw(|f| draw_status(f, f.size(), st)).unwrap();
        let buf = term.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.get(x, y).symbol());
            }
            out.push('\n');
        }
        out
    }
}
