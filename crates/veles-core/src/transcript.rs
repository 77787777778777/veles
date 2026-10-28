//! The transcript block model — ONE structure, live and on reload.
//!
//! ## What this replaces
//!
//! The desktop got this treatment in RM3 (`veles-desktop/lib/chat.tcl`:
//! one insert chokepoint, one block emitter) and the ground rules were
//! written down as executable data in `lib/transcriptspec.tcl`. The TUI
//! never got it. It kept a `Vec<Msg>` mutated from five independent
//! places — `push`, `append_delta`, `append_reasoning`, `finish_tool`,
//! `end_turn` — and the result was wrong by construction:
//!
//!   - `stream_target` was captured ONCE at turn start and never moved,
//!     so round two's prose was appended into round one's bubble while
//!     every tool call was pushed at the END. A turn rendered
//!     `reply(round1+round2) · tool · tool` instead of
//!     `reply · tool · reply`. The user's report: "in the transcript all
//!     the tool calls land at the bottom".
//!   - reasoning was INSERTED above the reply while streaming and
//!     deleted at turn end, but pushed BELOW the reply on reload. Live
//!     and replay disagreed about the same turn.
//!   - `role="tool"` rows were dropped entirely on reload, so a
//!     reopened session showed answers with no trace of the calls that
//!     produced them.
//!   - every `EngineNote` was flattened to one dim line with its
//!     `NoteKind` discarded, contradicting `notes.rs`' own docstring
//!     ("surfaces use this to pick a glyph/colour"), and injected rows
//!     replayed as ordinary user bubbles because the `injection` tag was
//!     thrown away.
//!
//! ## The rules (R1–R7, from `transcriptspec.tcl`)
//!
//!   R1  logical blocks are separated by a blank line
//!   R2  newlines inside output remain newlines (no welded blobs)
//!   R3  a tool round renders once — the same way live and on reload
//!   R4  reasoning collapses; it never vanishes
//!   R5  a patch is shown as diff lines
//!   R6  the live edge never orphans the view — follow survives a turn
//!   R7  every reasoning segment is its own block, live and reloaded
//!
//! R3 and R7 are the ones this module *is*: there is exactly one way to
//! add to a transcript (`Transcript::push`), and both the live path and
//! the reload path go through it. R1/R2/R5 are renderer properties; R6
//! belongs to whatever owns the viewport.
//!
//! ## The chokepoint
//!
//! `push` appends to the tail block when the tail is OPEN and of the
//! same kind, and otherwise opens a new one. Nothing inserts into the
//! middle, and no caller tracks an index. That single rule is what makes
//! `reply · tool · reply` come out in that order: a `ToolStart` closes
//! the open reply, so the next delta has to open a fresh one.

use crate::notes::EngineNote;

/// What a block is. The set is deliberately small: anything a surface
/// wants to style differently but every surface must still show.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Something the human said.
    User,
    /// Something the model said.
    Reply,
    /// One reasoning SEGMENT (R7) — not a turn's worth.
    Reasoning,
    /// One tool call: its header, and its result once it lands.
    Tool,
    /// An engine note (guard fired, compaction ran, failover…).
    Note,
    /// Surface-local chatter: command output, help, errors. Never
    /// persisted, so it exists live and not on reload — the one kind
    /// for which live ≠ replay is correct rather than a bug.
    Status,
}

/// A tool call, from the header to the result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tool {
    pub name: String,
    /// The arguments as they went on the wire, flattened to one line.
    /// The header is a single line BY CONSTRUCTION — this is the
    /// "welded blob" R2 warns about: a patch call whose arguments carry
    /// real newlines must not dump file fragments into the ⚙ line.
    pub args: String,
    /// `None` while the call is in flight.
    pub result: Option<String>,
    /// Joins a stored result row back to the assistant row that made
    /// the call (whose `tool_calls` carry the arguments). Without it a
    /// reopened session can only render "⚙ name".
    pub call_id: String,
}

/// The engine's own words, labelled (RM3-A4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub kind: crate::notes::NoteKind,
    pub summary: String,
    pub detail: String,
    /// `(key, value)` of the knob that silences this class, when there
    /// is one. Surfaces render it as a chip; the point is that no
    /// automatic behaviour is both invisible and unstoppable.
    pub toggle: Option<(String, String)>,
}

/// A row the ENGINE put in the conversation, not the user.
///
/// `[STEER] …`, `[BACKGROUND … COMPLETE]`, compaction context, todo
/// re-injection, verification nudges. These replayed as ordinary user
/// bubbles, styled exactly like something the human typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Injection {
    /// The class tag as `agent.rs` recorded it.
    pub class: String,
    /// The knob that mutes this class, when it has one.
    pub toggle: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: Kind,
    /// `hh:mm`, or empty when the source did not carry one.
    pub time: String,
    pub body: String,
    /// Still accepting appends. A block is closed by anything that
    /// opens a different one, and by `close_all` at turn end.
    pub open: bool,
    /// Reasoning blocks render as a header when collapsed; `body` holds
    /// the full text either way, so nothing ever vanishes (R4).
    pub collapsed: bool,
    pub tool: Option<Tool>,
    pub note: Option<Note>,
    pub injection: Option<Injection>,
}

impl Block {
    fn new(kind: Kind, time: String, body: String) -> Self {
        Self {
            kind,
            time,
            body,
            open: true,
            collapsed: false,
            tool: None,
            note: None,
            injection: None,
        }
    }
}

/// One thing that happened, from either the live stream or the store.
///
/// Both paths speak this vocabulary, which is what makes "live ≡ replay"
/// a property that can be TESTED rather than a habit that can drift.
#[derive(Debug)]
pub enum Event {
    User {
        text: String,
        time: String,
        injection: Option<Injection>,
    },
    /// A chunk of assistant prose.
    Delta(String),
    /// A chunk of reasoning.
    ReasoningDelta(String),
    ToolStart {
        name: String,
        args: String,
        call_id: String,
    },
    ToolEnd {
        name: String,
        result: String,
        call_id: String,
    },
    Note(Note),
    /// Surface-local text (command output). Live only.
    Status(String),
    /// A whole assistant message, already complete — the reload path,
    /// and the canonical text at the end of a live turn.
    Reply {
        text: String,
        time: String,
        reasoning: Option<String>,
    },
}

#[derive(Default, Clone)]
pub struct Transcript {
    pub blocks: Vec<Block>,
    /// A provider that emits content BEFORE its trailing reasoning
    /// closes the reply block mid-turn. `finish_reply` must replace
    /// that block in place rather than append a duplicate — this is
    /// its index, cleared by anything that restructures the tail.
    reply_gated_by_reasoning: Option<usize>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Close every open block.
    ///
    /// A reasoning segment collapses AS IT CLOSES, not at turn end: the
    /// one being written is expanded, a finished one is a header (R4 +
    /// R7). Doing it on the open→closed transition and nowhere else is
    /// what keeps a segment the user expanded by hand expanded — a
    /// blanket pass at turn end would fold it back up under them.
    pub fn close_all(&mut self) {
        for b in &mut self.blocks {
            if b.open {
                b.open = false;
                if b.kind == Kind::Reasoning {
                    b.collapsed = true;
                }
            }
        }
    }

    /// Is the tail an open block of this kind?
    fn tail_open(&self, kind: Kind) -> bool {
        matches!(self.blocks.last(), Some(b) if b.open && b.kind == kind)
    }

    /// THE CHOKEPOINT. Every addition to a transcript goes through here.
    ///
    /// Appending to an open tail of the same kind, and opening a new
    /// block otherwise, is the whole ordering rule: a `ToolStart` closes
    /// the reply that preceded it, so the model's next sentence lands in
    /// a NEW reply block below the tool rather than being welded onto
    /// the one above it.
    pub fn push(&mut self, ev: Event) {
        match ev {
            Event::User {
                text,
                time,
                injection,
            } => {
                self.close_all();
                self.reply_gated_by_reasoning = None;
                let mut b = Block::new(Kind::User, time, text);
                b.open = false;
                b.injection = injection;
                self.blocks.push(b);
            }

            Event::Delta(t) => {
                if self.tail_open(Kind::Reply) {
                    if let Some(b) = self.blocks.last_mut() {
                        b.body.push_str(&t);
                    }
                } else {
                    self.close_all();
                    self.reply_gated_by_reasoning = None;
                    self.blocks.push(Block::new(Kind::Reply, now_hms(), t));
                }
            }

            Event::ReasoningDelta(t) => {
                if self.tail_open(Kind::Reasoning) {
                    if let Some(b) = self.blocks.last_mut() {
                        b.body.push_str(&t);
                    }
                } else {
                    // R7: a reasoning chunk arriving after a tool round
                    // is a NEW segment, not a continuation of the last
                    // one. That falls out of the tail rule rather than
                    // being special-cased. When what it interrupts is
                    // an OPEN reply, remember that reply: the canonical
                    // text at turn end belongs IN it, not in a fresh
                    // block under the reasoning.
                    self.reply_gated_by_reasoning = if self.tail_open(Kind::Reply) {
                        self.blocks.len().checked_sub(1)
                    } else {
                        self.reply_gated_by_reasoning
                    };
                    self.close_all();
                    self.blocks.push(Block::new(Kind::Reasoning, now_hms(), t));
                }
            }

            Event::ToolStart {
                name,
                args,
                call_id,
            } => {
                self.close_all();
                self.reply_gated_by_reasoning = None;
                let mut b = Block::new(Kind::Tool, now_hms(), String::new());
                b.tool = Some(Tool {
                    name,
                    args: one_line(&args),
                    result: None,
                    call_id,
                });
                self.blocks.push(b);
            }

            Event::ToolEnd {
                name,
                result,
                call_id,
            } => {
                // The LAST in-flight call with this name (or id), not
                // "the tail": a second tool can start before the first
                // reports, and matching on the tail would then attach
                // the result to the wrong call.
                let hit = self.blocks.iter_mut().rev().find(|b| {
                    b.tool.as_ref().is_some_and(|t| {
                        t.result.is_none()
                            && (t.name == name || (!call_id.is_empty() && t.call_id == call_id))
                    })
                });
                match hit {
                    Some(b) => {
                        if let Some(t) = b.tool.as_mut() {
                            t.result = Some(result);
                        }
                        b.open = false;
                    }
                    None => {
                        // A result with no header — a reload of a
                        // session whose assistant row was compacted
                        // away. Render it rather than dropping it: a
                        // dropped tool row is how transcripts came to
                        // show answers with no trace of the work.
                        self.close_all();
                        let mut b = Block::new(Kind::Tool, now_hms(), String::new());
                        b.open = false;
                        b.tool = Some(Tool {
                            name,
                            args: String::new(),
                            result: Some(result),
                            call_id,
                        });
                        self.blocks.push(b);
                    }
                }
            }

            Event::Note(n) => {
                self.close_all();
                self.reply_gated_by_reasoning = None;
                let mut b = Block::new(Kind::Note, now_hms(), n.summary.clone());
                b.open = false;
                b.note = Some(n);
                self.blocks.push(b);
            }

            Event::Status(t) => {
                self.close_all();
                self.reply_gated_by_reasoning = None;
                let mut b = Block::new(Kind::Status, now_hms(), t);
                b.open = false;
                self.blocks.push(b);
            }

            Event::Reply {
                text,
                time,
                reasoning,
            } => {
                // Reasoning first: providers emit it before the content
                // it explains, so replay must place it there too — the
                // live path already does, and the two disagreeing about
                // the same turn was the bug.
                if let Some(r) = reasoning.filter(|r| !r.trim().is_empty()) {
                    self.close_all();
                    let mut b = Block::new(Kind::Reasoning, time.clone(), r);
                    b.open = false;
                    b.collapsed = true;
                    self.blocks.push(b);
                }
                self.close_all();
                let mut b = Block::new(Kind::Reply, time, text);
                b.open = false;
                self.blocks.push(b);
            }
        }
    }

    /// Replace the tail reply's body with the turn's canonical text.
    ///
    /// The stream is best-effort; `TurnResult::reply` is what was
    /// actually stored. The OPEN tail reply is replaced (the common
    /// order — providers emit reasoning before the prose it explains);
    /// when a provider emitted content first and a trailing reasoning
    /// segment closed it, that block is replaced in place instead, so
    /// the canonical text never lands twice.
    pub fn finish_reply(&mut self, text: &str) {
        let gated = self.reply_gated_by_reasoning.take();
        if let Some(b) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| b.kind == Kind::Reply && b.open)
        {
            b.body = text.to_string();
            b.open = false;
        } else if let Some(i) = gated.filter(|&i| {
            self.blocks
                .get(i)
                .is_some_and(|b| b.kind == Kind::Reply && !b.open)
        }) {
            self.blocks[i].body = text.to_string();
        } else if !text.is_empty() {
            let mut b = Block::new(Kind::Reply, now_hms(), text.to_string());
            b.open = false;
            self.blocks.push(b);
        }
        // Reasoning survives the turn, collapsed (USER decision,
        // 2026-09-07). It used to be DELETED here, which is why the
        // block you watched being written vanished the moment the turn
        // ended — and then reappeared, in a different position, on
        // reload. `close_all` is what collapses it.
        self.close_all();
    }
}

/// Build a transcript from stored rows — the RELOAD path.
///
/// This is the other half of "live ≡ replay": it converts the store's
/// vocabulary into the same `Event`s the stream produces, and pushes
/// them through the same chokepoint. There is no second layout
/// algorithm to drift from the first.
///
/// `notes` are anchored to the message they followed (`after_msg_id`),
/// so they land AT THE POINT THEY HAPPENED rather than in a heap at the
/// end — the difference between "a cron ran this turn" being visible
/// and a reopened session looking like a conversation you had.
///
/// `times` is parallel to `msgs`; pass an empty vec when the caller has
/// no timestamps (the desktop renders its own).
pub fn from_history(
    msgs: &[crate::providers::ChatMessage],
    ids: &[i64],
    times: &[String],
    notes: &[crate::store::StoredNote],
    injection_toggle: &dyn Fn(&str) -> Option<(String, String)>,
) -> Transcript {
    let mut t = Transcript::new();
    let mut ni = 0usize;
    let push_note = |t: &mut Transcript, n: &crate::store::StoredNote| {
        t.push(Event::Note(Note {
            // An unknown kind is a note from a newer build, not a
            // reason to drop the line: fall back to the neutral one.
            kind: crate::notes::NoteKind::parse(&n.kind).unwrap_or(crate::notes::NoteKind::Plugin),
            summary: n.summary.clone(),
            detail: n.detail.clone(),
            toggle: serde_json::from_str::<crate::notes::NoteToggle>(&n.toggle)
                .ok()
                .map(|t| (t.key, t.value)),
        }));
    };
    // Notes pinned before any message come first.
    while ni < notes.len() && notes[ni].after_msg_id <= 0 {
        let from = t.blocks.len();
        push_note(&mut t, &notes[ni]);
        stamp_from(&mut t, from, &local_hms(&notes[ni].created_at));
        ni += 1;
    }
    for (i, m) in msgs.iter().enumerate() {
        let time = local_hms(&times.get(i).cloned().unwrap_or_default());
        let from = t.blocks.len();
        match m.role.as_str() {
            "tool" => {
                let (name, call_id) = match &m.tool_meta {
                    Some(tm) => (tm.name.clone(), tm.tool_call_id.clone()),
                    None => (String::new(), String::new()),
                };
                t.push(Event::ToolEnd {
                    name,
                    result: m.content.clone(),
                    call_id,
                });
            }
            "assistant" => {
                // Chronology: the model reasons, writes its prose, THEN
                // makes the calls — one assistant row per round, so
                // restoring that order is a per-row decision. The call
                // HEADERS come after the prose (the only persistent
                // copy of the arguments; without it a reopened session
                // can render "⚙ name" and nothing else), and a tool
                // row further down attaches the result to its header.
                if !m.content.trim().is_empty() || m.reasoning.is_some() {
                    t.push(Event::Reply {
                        text: m.content.clone(),
                        time: time.clone(),
                        reasoning: m.display_reasoning.clone().or_else(|| m.reasoning.clone()),
                    });
                }
                for (name, args, call_id) in tool_calls_of(m) {
                    t.push(Event::ToolStart {
                        name,
                        args,
                        call_id,
                    });
                }
            }
            "user" => t.push(Event::User {
                text: m.content.clone(),
                time: time.clone(),
                injection: m.injection.as_ref().map(|c| Injection {
                    class: c.clone(),
                    toggle: injection_toggle(c),
                }),
            }),
            // system/session_meta rows are bookkeeping, not transcript.
            _ => {}
        }
        // Every block this row opened wears the row's own time — the
        // tool headers an assistant row makes included. They used to
        // take the live path's wall clock, which on a reload is the
        // moment of the RELOAD: every call in a reopened session was
        // stamped "just now".
        stamp_from(&mut t, from, &time);
        let this_id = ids.get(i).copied().unwrap_or(i64::MAX);
        while ni < notes.len() && notes[ni].after_msg_id <= this_id {
            let from = t.blocks.len();
            push_note(&mut t, &notes[ni]);
            stamp_from(&mut t, from, &local_hms(&notes[ni].created_at));
            ni += 1;
        }
    }
    for n in &notes[ni.min(notes.len())..] {
        let from = t.blocks.len();
        push_note(&mut t, n);
        stamp_from(&mut t, from, &local_hms(&n.created_at));
    }
    t.close_all();
    t
}

/// `(name, args, call_id)` for each call an assistant row made.
fn tool_calls_of(m: &crate::providers::ChatMessage) -> Vec<(String, String, String)> {
    let Some(arr) = m.tool_calls.as_ref().and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .map(|c| {
            let f = &c["function"];
            (
                f["name"].as_str().unwrap_or("?").to_string(),
                f["arguments"].as_str().unwrap_or_default().to_string(),
                c["id"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// Flatten to one line: a header is a single line by construction.
fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

/// HH:MM:SS — to the second (USER 2026-09-30, of the desktop's HH:MM:
/// "don't show seconds"; a tool loop does several things a minute).
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// A stored `created_at` — SQLite's `YYYY-MM-DD HH:MM:SS`, in UTC — as
/// this machine's local `HH:MM:SS`, the way the live path stamps. A
/// reloaded row used to show the raw UTC string. Anything that is not
/// such a stamp comes back unchanged (an already-formatted time), and an
/// empty one stays empty: no stamp rather than an invented one.
pub fn local_hms(created_at: &str) -> String {
    chrono::NaiveDateTime::parse_from_str(created_at.trim(), "%Y-%m-%d %H:%M:%S")
        .map(|t| {
            t.and_utc()
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|_| created_at.trim().to_string())
}

/// Stamp the blocks from index `from` on with `time`, when there is one.
fn stamp_from(t: &mut Transcript, from: usize, time: &str) {
    if time.is_empty() {
        return;
    }
    for b in t.blocks.iter_mut().skip(from) {
        b.time = time.to_string();
    }
}

/// The engine's note, as a transcript note.
pub fn note_from_engine(n: &EngineNote) -> Note {
    Note {
        kind: n.kind,
        summary: n.summary.clone(),
        detail: n.detail.clone(),
        toggle: n.toggle.as_ref().map(|t| (t.key.clone(), t.value.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(t: &Transcript) -> Vec<Kind> {
        t.blocks.iter().map(|b| b.kind).collect()
    }

    fn bodies(t: &Transcript) -> Vec<String> {
        t.blocks
            .iter()
            .map(|b| match &b.tool {
                Some(tool) => format!("⚙{}", tool.name),
                None => b.body.clone(),
            })
            .collect()
    }

    /// Feed a two-round turn: prose, a tool, more prose.
    fn two_round_turn(t: &mut Transcript) {
        t.push(Event::User {
            text: "do the thing".into(),
            time: "12:00".into(),
            injection: None,
        });
        t.push(Event::Delta("let me look".into()));
        t.push(Event::ToolStart {
            name: "read_file".into(),
            args: "{\"path\":\"a\"}".into(),
            call_id: "c1".into(),
        });
        t.push(Event::ToolEnd {
            name: "read_file".into(),
            result: "contents".into(),
            call_id: "c1".into(),
        });
        t.push(Event::Delta("here is the answer".into()));
    }

    /// THE REPORTED BUG. Tool calls used to collect at the bottom
    /// because `stream_target` was fixed at turn start: round two's
    /// prose was appended into round one's bubble, ABOVE every tool.
    #[test]
    fn a_two_round_turn_renders_reply_tool_reply_in_that_order() {
        let mut t = Transcript::new();
        two_round_turn(&mut t);
        assert_eq!(
            kinds(&t),
            vec![Kind::User, Kind::Reply, Kind::Tool, Kind::Reply],
            "the tool belongs BETWEEN the two replies, not after both"
        );
        assert_eq!(
            bodies(&t),
            vec![
                "do the thing",
                "let me look",
                "⚙read_file",
                "here is the answer"
            ],
            "round two's prose must not be welded onto round one's"
        );
    }

    /// R7: a reasoning chunk after a tool round opens its own segment.
    #[test]
    fn every_reasoning_segment_is_its_own_block() {
        let mut t = Transcript::new();
        t.push(Event::ReasoningDelta("first I check".into()));
        t.push(Event::ToolStart {
            name: "terminal".into(),
            args: "ls".into(),
            call_id: "c1".into(),
        });
        t.push(Event::ToolEnd {
            name: "terminal".into(),
            result: "ok".into(),
            call_id: "c1".into(),
        });
        t.push(Event::ReasoningDelta("now I know".into()));
        assert_eq!(
            kinds(&t),
            vec![Kind::Reasoning, Kind::Tool, Kind::Reasoning],
            "the second segment must not reopen the first"
        );
        assert_eq!(t.blocks[0].body, "first I check");
        assert_eq!(t.blocks[2].body, "now I know");
    }

    /// Deltas of one segment concatenate; that is the only thing that
    /// appends (R2 — the newlines inside them survive).
    #[test]
    fn deltas_of_one_segment_concatenate_and_keep_newlines() {
        let mut t = Transcript::new();
        t.push(Event::Delta("line one\n".into()));
        t.push(Event::Delta("line two".into()));
        assert_eq!(kinds(&t), vec![Kind::Reply]);
        assert_eq!(t.blocks[0].body, "line one\nline two");
    }

    /// R4: reasoning collapses at turn end. It never vanishes — which
    /// it did, for as long as the TUI deleted the block outright.
    #[test]
    fn reasoning_survives_the_turn_collapsed() {
        let mut t = Transcript::new();
        t.push(Event::ReasoningDelta("thinking".into()));
        t.push(Event::Delta("answer".into()));
        t.finish_reply("answer");
        let r = t
            .blocks
            .iter()
            .find(|b| b.kind == Kind::Reasoning)
            .expect("reasoning must still be there after the turn");
        assert!(r.collapsed, "collapsed, per the user's choice");
        assert_eq!(r.body, "thinking", "and the text is still readable");
    }

    /// R3, the property this module exists for: the same turn, fed live
    /// as deltas or replayed as whole messages, lays out identically.
    #[test]
    fn live_and_replay_lay_out_the_same_turn_identically() {
        let mut live = Transcript::new();
        two_round_turn(&mut live);
        live.finish_reply("here is the answer");

        // The store's version of the same turn: a user row, an
        // assistant row that made the call, the tool result, then the
        // final assistant row.
        let mut replay = Transcript::new();
        replay.push(Event::User {
            text: "do the thing".into(),
            time: "12:00".into(),
            injection: None,
        });
        replay.push(Event::Reply {
            text: "let me look".into(),
            time: "12:00".into(),
            reasoning: None,
        });
        replay.push(Event::ToolStart {
            name: "read_file".into(),
            args: "{\"path\":\"a\"}".into(),
            call_id: "c1".into(),
        });
        replay.push(Event::ToolEnd {
            name: "read_file".into(),
            result: "contents".into(),
            call_id: "c1".into(),
        });
        replay.push(Event::Reply {
            text: "here is the answer".into(),
            time: "12:00".into(),
            reasoning: None,
        });

        assert_eq!(kinds(&live), kinds(&replay), "block kinds must match");
        assert_eq!(bodies(&live), bodies(&replay), "block bodies must match");
    }

    /// A tool result whose header was compacted away still renders.
    #[test]
    fn an_orphan_tool_result_is_shown_not_dropped() {
        let mut t = Transcript::new();
        t.push(Event::ToolEnd {
            name: "terminal".into(),
            result: "exit 0".into(),
            call_id: "gone".into(),
        });
        assert_eq!(kinds(&t), vec![Kind::Tool]);
        assert_eq!(
            t.blocks[0].tool.as_ref().and_then(|x| x.result.as_deref()),
            Some("exit 0")
        );
    }

    /// Two calls in flight at once: each result finds ITS call.
    #[test]
    fn a_result_attaches_to_its_own_call_not_the_tail() {
        let mut t = Transcript::new();
        t.push(Event::ToolStart {
            name: "a".into(),
            args: String::new(),
            call_id: "c1".into(),
        });
        t.push(Event::ToolStart {
            name: "b".into(),
            args: String::new(),
            call_id: "c2".into(),
        });
        t.push(Event::ToolEnd {
            name: "a".into(),
            result: "A".into(),
            call_id: "c1".into(),
        });
        let a = t.blocks[0].tool.as_ref().expect("first call");
        let b = t.blocks[1].tool.as_ref().expect("second call");
        assert_eq!(a.result.as_deref(), Some("A"), "the result found call a");
        assert_eq!(b.result, None, "call b is still in flight");
    }

    /// A provider that emits content BEFORE its trailing reasoning
    /// closes the reply mid-turn; the canonical text must replace that
    /// block, not append a second copy under the reasoning.
    #[test]
    fn trailing_reasoning_does_not_duplicate_the_reply() {
        let mut t = Transcript::new();
        t.push(Event::Delta("here is the answer".into()));
        t.push(Event::ReasoningDelta("…or is it".into()));
        t.finish_reply("here is the answer");
        let replies = t.blocks.iter().filter(|b| b.kind == Kind::Reply).count();
        assert_eq!(replies, 1, "one reply, not two");
        assert_eq!(kinds(&t), vec![Kind::Reply, Kind::Reasoning]);
        assert!(t.blocks[1].collapsed, "the reasoning collapsed");
    }

    /// R2: a tool header is one line even when the arguments carry
    /// real newlines — the "welded blob" a patch call used to produce.
    #[test]
    fn a_tool_header_is_one_line_however_the_args_arrived() {
        let mut t = Transcript::new();
        t.push(Event::ToolStart {
            name: "patch".into(),
            args: "{\"old\":\"a\nb\nc\"}".into(),
            call_id: String::new(),
        });
        let args = &t.blocks[0].tool.as_ref().expect("tool").args;
        assert!(!args.contains('\n'), "header must not wrap: {args:?}");
    }

    /// An injected row carries its class, so a surface can label it
    /// instead of rendering the engine's words as the user's.
    #[test]
    fn an_injected_row_keeps_its_class() {
        let mut t = Transcript::new();
        t.push(Event::User {
            text: "[STEER] use Python".into(),
            time: "12:00".into(),
            injection: Some(Injection {
                class: "steer".into(),
                toggle: None,
            }),
        });
        assert_eq!(
            t.blocks[0].injection.as_ref().map(|i| i.class.as_str()),
            Some("steer"),
            "the tag must survive into the block, live and on reload"
        );
    }

    /// A note keeps its kind, so a surface can pick a glyph — which is
    /// what `notes.rs` says surfaces do, and what the TUI did not.
    #[test]
    fn a_note_keeps_its_kind_and_its_toggle() {
        let mut t = Transcript::new();
        let n = EngineNote::new(crate::notes::NoteKind::Guard, "repeat guard fired")
            .with_detail("three identical calls");
        t.push(Event::Note(note_from_engine(&n)));
        let b = &t.blocks[0];
        assert_eq!(b.kind, Kind::Note);
        assert_eq!(
            b.note.as_ref().map(|x| x.kind),
            Some(crate::notes::NoteKind::Guard)
        );
        assert_eq!(
            b.note.as_ref().map(|x| x.detail.as_str()),
            Some("three identical calls")
        );
    }

    /// The glyph is the surface's to add (the TUI picks it from the
    /// kind). A live note carried `line()`, glyph included, so the TUI
    /// printed `🧠 🧠 444 chars of context injected`; a reloaded one
    /// carried the bare summary. Both must read the same.
    #[test]
    fn a_live_note_carries_the_bare_summary_like_a_reloaded_one() {
        let n = EngineNote::new(
            crate::notes::NoteKind::Memory,
            "444 chars of context injected",
        );
        assert_eq!(
            note_from_engine(&n).summary,
            "444 chars of context injected"
        );
    }

    #[test]
    fn a_reloaded_row_keeps_its_own_time() {
        use crate::providers::ChatMessage;
        let calls = serde_json::json!([{
            "id": "c1", "type": "function",
            "function": { "name": "read_file", "arguments": "{\"path\":\"a\"}" }
        }]);
        let mut call_row = ChatMessage::assistant("let me look", None);
        call_row.tool_calls = Some(calls);
        let msgs = vec![ChatMessage::user("do the thing"), call_row];
        let times = vec![
            "2026-09-07 01:23:45".to_string(),
            "2026-09-07 01:24:10".to_string(),
        ];
        let t = from_history(&msgs, &[1, 2], &times, &[], &|_| None);
        let stamps: Vec<(Kind, String)> =
            t.blocks.iter().map(|b| (b.kind, b.time.clone())).collect();
        let local = |s: &str| local_hms(s);
        assert_eq!(
            stamps,
            vec![
                (Kind::User, local("2026-09-07 01:23:45")),
                (Kind::Reply, local("2026-09-07 01:24:10")),
                (Kind::Tool, local("2026-09-07 01:24:10")),
            ],
            "every block wears its row's time, in local time — the call too, not the reload's clock"
        );
    }

    #[test]
    fn a_stored_time_is_stamped_in_local_time_to_the_second() {
        let want =
            chrono::NaiveDateTime::parse_from_str("2026-09-07 01:23:45", "%Y-%m-%d %H:%M:%S")
                .unwrap()
                .and_utc()
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string();
        assert_eq!(local_hms("2026-09-07 01:23:45"), want);
        assert_eq!(local_hms(""), "", "no time, no stamp");
        assert_eq!(local_hms("12:00"), "12:00", "already a time: unchanged");
    }
}
