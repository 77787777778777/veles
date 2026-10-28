//! Engine notes — every implicit action says so, in the transcript.
//!
//! USER DIRECTIVE: "any guards and other implicit mechanisms should
//! actually always be explicitly shown in the transcript somehow because
//! that's one of the most infuriating things about hermes-agent when the
//! already-inscrutable pile of slop just does shit secretly without you
//! knowing".
//!
//! The rule: if the engine takes an action the user did not type, the
//! user must be able to see that it happened, where it happened. Silent
//! automation is the defect — not the mechanism.
//!
//! ## Why this rides `StreamEvent`
//!
//! [`crate::providers::StreamEvent`] is already the ONE channel a turn
//! speaks through, and `Agent::run_turn` already takes a single
//! `FnMut(StreamEvent)` sink. The surfaces use wildcard arms on that
//! enum, so the compiler does NOT force them to handle a new variant —
//! this note exists as the enforcement the type system cannot give:
//! every surface renders `Note` (CLI prints it, TUI pushes a status
//! line, desktop SSE streams it, the chat-platform gateway rides it on
//! the final reply). A side channel — a log line, a `tracing::info!`,
//! an extra field somewhere — is exactly how these mechanisms became
//! invisible in the first place: it is possible to not handle it, so
//! nobody did. A new surface that drops `Note` reintroduces silent
//! automation — the defect this module exists to prevent.
//!
//! ## Shape
//!
//! One line always (`summary`), full story on demand (`detail`), and —
//! where the mechanism can be turned off or reconfigured — the knob to
//! do it (`toggle`), so the control is reachable from the place you
//! noticed you wanted it.

use serde::{Deserialize, Serialize};

/// Which mechanism spoke. Surfaces use this to pick a glyph/colour and
/// users use it to filter, so the set is deliberately small and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// Harness guards: iteration cap, repeat guardrail, verification nudge.
    Guard,
    /// Context compaction rewrote the history.
    Compaction,
    /// A route other than the primary answered.
    Failover,
    /// The approval prefilter decided something on the user's behalf.
    Approval,
    /// Aux work (titles, summaries, subagents) ran on a different route.
    AuxRoute,
    /// A `[[model.binds]]` rule chose this SESSION's primary route.
    /// Distinct from `AuxRoute` (which is about side work) and from
    /// `Failover` (which is about a route that stopped answering): this
    /// is the main route, chosen for you at session creation by a rule
    /// you wrote once and will not be looking at now.
    Route,
    /// Memory was injected into the system prompt.
    Memory,
    /// A plugin rewrote what is displayed.
    PluginRewrite,
    /// A plugin TOOK AN ENGINE ACTION (an LLM call, an outbound
    /// message, a tool/command registration). Silent automation is
    /// prohibited — every act a plugin performs announces itself here.
    Plugin,
    /// A turn ran with no user present (cron, dreaming).
    Schedule,
    /// The provider's prefix cache stopped hitting — a round that used
    /// to be served from cache is now paying full price, and (when we
    /// can tell) which block changed to break it.
    Cache,
    /// The engine injected a synthetic user-role message into the
    /// context (background event, todo re-injection, hook context,
    /// verification nudge). USER: "I wouldn't mind them so much if I
    /// could see when they were happening explicitly and manage them
    /// from the transcript" — this is the seeing; the note's toggle
    /// is the managing.
    Injection,
    /// A model wrote hidden deliberation into its ANSWER field (a
    /// think-tag pair or pipe token) rather than into the reasoning
    /// field; the engine moved it to reasoning so it is not replayed
    /// and billed as answer. The move is announced, never silent.
    Reasoning,
}

impl NoteKind {
    /// Stable identifier, used for config keys and the wire format.
    pub fn as_str(&self) -> &'static str {
        match self {
            NoteKind::Guard => "guard",
            NoteKind::Compaction => "compaction",
            NoteKind::Failover => "failover",
            NoteKind::Approval => "approval",
            NoteKind::AuxRoute => "aux_route",
            NoteKind::Route => "route",
            NoteKind::Memory => "memory",
            NoteKind::PluginRewrite => "plugin_rewrite",
            NoteKind::Plugin => "plugin",
            NoteKind::Schedule => "schedule",
            NoteKind::Cache => "cache",
            NoteKind::Injection => "injection",
            NoteKind::Reasoning => "reasoning",
        }
    }

    /// A short marker for text surfaces that cannot do colour.
    pub fn glyph(&self) -> &'static str {
        match self {
            NoteKind::Guard => "⚠",
            NoteKind::Compaction => "⇲",
            NoteKind::Failover => "⇄",
            NoteKind::Approval => "⚖",
            NoteKind::AuxRoute => "↗",
            NoteKind::Route => "⇢",
            NoteKind::Memory => "🧠",
            NoteKind::PluginRewrite => "✎",
            NoteKind::Plugin => "⚙",
            NoteKind::Schedule => "⏱",
            NoteKind::Cache => "≡",
            NoteKind::Injection => "⇒",
            NoteKind::Reasoning => "💭",
        }
    }

    pub fn all() -> &'static [NoteKind] {
        &[
            NoteKind::Guard,
            NoteKind::Compaction,
            NoteKind::Failover,
            NoteKind::Approval,
            NoteKind::AuxRoute,
            NoteKind::Route,
            NoteKind::Memory,
            NoteKind::PluginRewrite,
            NoteKind::Plugin,
            NoteKind::Schedule,
            NoteKind::Cache,
            NoteKind::Injection,
            NoteKind::Reasoning,
        ]
    }

    /// Deliberately NOT `FromStr`: that trait returns `Result`, and an
    /// unknown mechanism name is an ordinary absence here, not an error.
    pub fn parse(s: &str) -> Option<NoteKind> {
        NoteKind::all().iter().copied().find(|k| k.as_str() == s)
    }
}

/// The control offered alongside a note.
///
/// Seeing that something happened is half of it; being able to stop it
/// happening, from where you saw it, is the other half.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteToggle {
    /// Mechanism key — matches [`NoteKind::as_str`] for on/off knobs, or
    /// a dotted config path for finer ones (`agent.guards.max_iterations`).
    pub key: String,
    /// What it is set to right now, rendered for a human.
    pub value: String,
    /// The values a surface may offer. Empty = free-form.
    pub choices: Vec<String>,
    /// One line telling the user what changing it does.
    pub help: String,
}

impl NoteToggle {
    /// An on/off switch for a whole mechanism.
    pub fn on_off(kind: NoteKind, enabled: bool, help: impl Into<String>) -> Self {
        NoteToggle {
            key: kind.as_str().to_string(),
            value: if enabled { "on" } else { "off" }.to_string(),
            choices: vec!["on".into(), "off".into()],
            help: help.into(),
        }
    }
}

/// One thing the engine did that the user did not ask for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineNote {
    pub kind: NoteKind,
    /// ONE line. Always shown, inline, at the point it happened.
    /// Say what changed, not that something changed: "compacted 86k→1k
    /// chars" beats "history was compacted".
    pub summary: String,
    /// The full story, shown on demand. Empty when the summary is all
    /// there is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// The knob, when this mechanism has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toggle: Option<NoteToggle>,
    /// The persisted engine_notes id, set by `persist_note` when the
    /// note was saved. 0 means this copy predates persistence (a
    /// record that never writes it is not a lie: 0 is "unknown", not
    /// "first"). Clients advance their note cursor by it, so a note
    /// that arrived live can never be replayed at the next reattach.
    #[serde(default, skip_serializing_if = "is_zero_note_id")]
    pub id: i64,
}

fn is_zero_note_id(v: &i64) -> bool {
    *v == 0
}

impl EngineNote {
    pub fn new(kind: NoteKind, summary: impl Into<String>) -> Self {
        EngineNote {
            kind,
            summary: summary.into(),
            detail: String::new(),
            toggle: None,
            id: 0,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn with_toggle(mut self, toggle: NoteToggle) -> Self {
        self.toggle = Some(toggle);
        self
    }

    /// The single line a text surface prints.
    pub fn line(&self) -> String {
        format!("{} {}", self.kind.glyph(), self.summary)
    }
}

// ── per-session mechanism overrides ─────────────────────────────────
//
// Config files hold the defaults; a session can override them without
// editing anything on disk. Stored in the SAME place `/failover`
// already keeps its per-session mode (store `meta`), so `--resume`
// keeps them and there is one persistence mechanism, not two.

/// `meta` key prefix for a per-session mechanism override.
pub const META_PREFIX: &str = "mechanism:";

/// The knob name for a mechanism, as `/mechanisms` and the note toggles
/// use it.
pub fn meta_key(session_id: &str, key: &str) -> String {
    format!("{META_PREFIX}{session_id}:{key}")
}

/// Every mechanism, its default, and what it does — the table
/// `/mechanisms` renders and the overlay lists.
pub struct MechanismInfo {
    pub kind: NoteKind,
    /// Dotted config path this maps to.
    pub config_key: &'static str,
    pub help: &'static str,
}

pub fn catalogue() -> &'static [MechanismInfo] {
    &[
        MechanismInfo {
            kind: NoteKind::Guard,
            config_key: "agent.guards",
            help: "iteration cap, identical-repeat guardrail, verification nudge",
        },
        MechanismInfo {
            kind: NoteKind::Compaction,
            config_key: "agent.compaction",
            help: "summarize + archive old history when the window fills",
        },
        MechanismInfo {
            kind: NoteKind::Failover,
            config_key: "failover",
            help: "what happens when a provider route fails",
        },
        MechanismInfo {
            kind: NoteKind::Approval,
            config_key: "approvals",
            help: "aux-model screening of risky tool calls before you see them",
        },
        MechanismInfo {
            kind: NoteKind::Route,
            config_key: "model.binds",
            help: "a [[model.binds]] rule choosing this session's route at creation",
        },
        MechanismInfo {
            kind: NoteKind::AuxRoute,
            config_key: "aux_fallback",
            help: "titles/summaries/subagents moving to another provider",
        },
        MechanismInfo {
            kind: NoteKind::Memory,
            config_key: "memory",
            help: "MEMORY/USER content injected into the system prompt",
        },
        MechanismInfo {
            kind: NoteKind::PluginRewrite,
            config_key: "plugins",
            help: "Tcl plugins rewriting displayed output",
        },
        MechanismInfo {
            kind: NoteKind::Plugin,
            config_key: "plugins",
            help:
                "actions plugins take on the engine (LLM calls, outbound messages, registrations)",
        },
        MechanismInfo {
            kind: NoteKind::Schedule,
            config_key: "cron",
            help: "turns that run with no user present (cron, dreaming)",
        },
        MechanismInfo {
            kind: NoteKind::Cache,
            config_key: "cache",
            help: "prompt-cache hit-rate accounting and miss warnings",
        },
        MechanismInfo {
            kind: NoteKind::Injection,
            config_key: "agent.injections",
            help: "engine messages the harness injects as user turns (background, todo, hook)",
        },
        MechanismInfo {
            kind: NoteKind::Reasoning,
            config_key: "agent.reasoning",
            help: "a model thinking into its answer field instead of the reasoning field",
        },
    ]
}

/// Human-readable listing for `/mechanisms` with no arguments.
///
/// Deliberately lists EVERY mechanism, including the ones that have not
/// fired this session — the point is to be able to find out what the
/// engine can do behind your back, not only what it happened to do.
pub fn render_catalogue(
    overrides: &[(String, String)],
    mechanisms: &crate::config::MechanismsSection,
) -> String {
    let mut out = String::from("mechanisms (what the engine may do on its own):\n");
    for m in catalogue() {
        let key = m.kind.as_str();
        let ov = overrides
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str());
        // Three layers, named. "config default" alone could not answer
        // the question people actually arrive with — "why am I not
        // seeing guard notes" — because it did not say what the default
        // WAS, let alone that a connector might be trimming them.
        let state = match ov {
            Some(v) => format!("{v} (session)"),
            None if mechanisms.silences(key) => "off (config)".to_string(),
            None => "on".to_string(),
        };
        out.push_str(&format!(
            "  {} {:<15} {:<24} {}\n",
            m.kind.glyph(),
            key,
            state,
            m.help
        ));
    }
    // The per-connector layer. Listed rather than folded into the rows
    // above because it is not a fourth state of each mechanism — it is
    // a property of one SURFACE, and the mechanism still ran and is
    // still in the transcript for every other surface to show.
    //
    // Shipped connectors are listed even with nothing configured: since
    // 2026-09-10 an unconfigured irc/telegram silences the bookkeeping
    // kinds by default, and a readout that skipped platforms nobody wrote
    // a section for would hide the only layer actually in force — which
    // is the exact question this renderer exists to answer ("why am I not
    // seeing guard notes").
    let mut names: Vec<String> = mechanisms.platforms.keys().cloned().collect();
    for c in crate::config::CONNECTORS {
        if !names.iter().any(|n| n == c) {
            names.push((*c).to_string());
        }
    }
    names.sort();
    names.retain(|n| !n.is_empty());
    for name in &names {
        let p = mechanisms.platforms.get(name);
        let v = mechanisms.verbosity(name);
        let default_on = mechanisms.connector_default_applies(name);
        let own = p.map(|p| p.silenced.as_slice()).unwrap_or(&[]);
        if v == crate::config::Verbosity::Normal && own.is_empty() && !default_on {
            continue;
        }
        let mut what = Vec::new();
        if v != crate::config::Verbosity::Normal {
            what.push(format!("verbosity {}", v.as_str()));
        }
        if !own.is_empty() {
            what.push(format!("silenced: {}", own.join(" ")));
        }
        if default_on {
            what.push(format!(
                "silenced by default: {} (verbosity = \"normal\" here restores them)",
                crate::config::CONNECTOR_DEFAULT_SILENCED.join(" ")
            ));
        }
        out.push_str(&format!(
            "  on {name}: {} (that surface's copy only — the transcript keeps them)\n",
            what.join(" · ")
        ));
    }
    out.push_str("\n  /mechanisms <name> off|on   override for this session\n");
    out.push_str("  /mechanisms <name> default  drop the override\n");
    out.push_str("  [mechanisms] silenced / [mechanisms.platforms.<p>] in config.toml\n");
    out
}

/// Aux LLM calls that ran on a route OTHER than the conversation's.
///
/// Titles, approval screens, reasoning summaries and plugin
/// `veles::call_llm` calls can be routed elsewhere — `failover`'s
/// `aux_fallback` deliberately moves them off a throttled scope. That is
/// a good mechanism, and it is exactly the kind that must not be silent:
/// work is being billed to a provider the user did not pick for this
/// conversation.
///
/// The producers (bridge.rs, titles.rs) are process-global and have no
/// session or event sink, so they PARK the fact here and `Agent`
/// drains it at the next point it is emitting. Same shape as the
/// agent's own `pending_notes`, one level up.
static AUX_USED: std::sync::Mutex<Vec<(String, String, String)>> =
    std::sync::Mutex::new(Vec::new());

/// Record that `purpose` ran on `provider`/`model`. Callers should only
/// call this when the route DIFFERS from the conversation's primary —
/// aux work on the same route is not a diversion worth reporting.
pub fn record_aux_route(purpose: &str, provider: &str, model: &str) {
    if let Ok(mut v) = AUX_USED.lock() {
        // Bounded: a plugin in a loop must not grow this without limit.
        if v.len() < 32 {
            v.push((purpose.to_string(), provider.to_string(), model.to_string()));
        }
    }
}

// ── prefix-cache miss accounting ──────────────────────────────────────
//
// A prefix cache keys on the BYTES of the request prefix. When the hit
// rate drops between two rounds, the honest question is "what changed?"
// — and the answer is a diff of the blocks that make up the prefix.

/// Describe what changed between two rounds' prefix blocks, so a cache
/// miss note can NAME the block that grew/shrank/appeared instead of
/// just saying "cache miss". Returns None when the blocks are identical
/// (the miss came from somewhere the block list can't see — e.g. the
/// history tail, or the provider evicting its own cache).
pub fn prefix_block_diff(prev: &[(String, usize)], curr: &[(String, usize)]) -> Option<String> {
    let mut changes: Vec<String> = Vec::new();
    for (name, n) in curr {
        match prev.iter().find(|(pn, _)| pn == name) {
            Some((_, pn)) if pn != n => {
                changes.push(format!("{name} grew {}→{} chars", pn, n));
            }
            None => changes.push(format!("{name} appeared ({} chars)", n)),
            _ => {}
        }
    }
    for (name, n) in prev {
        if !curr.iter().any(|(cn, _)| cn == name) {
            changes.push(format!("{name} dropped (was {} chars)", n));
        }
    }
    if changes.is_empty() {
        None
    } else {
        Some(changes.join(", "))
    }
}

/// A one-line fingerprint of a history's TAIL — the last few rows, each
/// tagged with its ABSOLUTE row number, by role/tool and length.
///
/// The prefix blocks above are the system prompt and tool schemas: the
/// part of the request BEFORE any history. A prefix cache covers the
/// whole request prefix, history included, so when the blocks are
/// unchanged and the hit rate still dropped, "history rewrite or
/// provider-side eviction" was the honest-but-unhelpful answer — it named
/// two possibilities and distinguished neither.
///
/// The mechanism that makes the second possibility rare and the first
/// real: a tool result is capped at `tool_output_cap` before it enters
/// history, and history is what every later round re-sends. Anything that
/// rewrites a row the earlier round already sent (a re-cap, a re-render,
/// a compaction) changes the bytes from the first rewritten row onward
/// and invalidates the cache from exactly there — with an identical
/// prefix, which is the shape that was reporting itself as eviction.
///
/// The row NUMBER is part of the fingerprint, not decoration. This is a
/// sliding window over a growing list: once history grows past the
/// window, the same row sits at a different OFFSET in the newer window,
/// and comparing two windows positionally would report the shift itself
/// as a rewrite. Absolute numbers make the alignment exact.
///
/// Deliberately O(tail), not a hash of the whole history: this runs only
/// on a miss, and the useful statement is "row N changed shape", not a
/// digest nobody can read.
pub fn history_tail_fingerprint(rows: &[(usize, String, String, usize)]) -> Vec<(usize, String)> {
    rows.iter()
        .map(|(idx, role, name, chars)| {
            let desc = if name.is_empty() {
                format!("{role}:{chars}")
            } else {
                format!("{role}/{name}:{chars}")
            };
            (*idx, desc)
        })
        .collect()
}

/// Name the row whose shape changed between two rounds, for the
/// cache-miss note. `None` when nothing the earlier round sent was
/// rewritten and nothing was appended either.
pub fn history_tail_diff(prev: &[(usize, String)], curr: &[(usize, String)]) -> Option<String> {
    // A REWRITE: a row the earlier round already sent, by absolute
    // number, whose shape is now different. Lowest number first — the
    // cache dies from the first rewritten row onward, so that is the one
    // worth naming.
    for (idx, before) in prev {
        if let Some((_, after)) = curr.iter().find(|(i, _)| i == idx) {
            if before != after {
                return Some(format!(
                    "history row {} changed shape ({before} → {after}) — a row \
                     the earlier round already sent was rewritten",
                    idx + 1
                ));
            }
        }
    }
    let prev_max = prev.iter().map(|(i, _)| *i).max();
    let curr_max = curr.iter().map(|(i, _)| *i).max();
    match (prev_max, curr_max) {
        // APPEND: history grew past what the cache had seen. Not a
        // rewrite — the new rows are simply what no cache could cover.
        (Some(p), Some(c)) if c > p => Some(format!(
            "history grew by {} row(s) — the new tail is what the cache \
             could not cover",
            c - p
        )),
        // The request the earlier round sent is no longer a prefix of
        // this one: rows went away (a compaction, a truncation).
        (Some(p), Some(c)) if c < p => Some(format!(
            "history lost {} row(s) from its tail — the request the cache \
             saw is no longer a prefix of this one",
            p - c
        )),
        _ => None,
    }
}

/// Take everything recorded since the last call, as ready-made notes.
pub fn take_aux_notes() -> Vec<EngineNote> {
    let taken = AUX_USED
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default();
    taken
        .into_iter()
        .map(|(purpose, provider, model)| {
            EngineNote::new(
                NoteKind::AuxRoute,
                format!("{purpose} ran on {provider}/{model}, not this conversation's route"),
            )
            .with_detail(
                "Aux work (titles, approval screens, reasoning summaries, plugin \
                 LLM calls) can be pointed at a different provider so it does not \
                 compete with the conversation for the same rate limit. \
                 `failover_rules`' aux_fallback_provider/model in config.toml \
                 controls it.",
            )
            .with_toggle(NoteToggle {
                key: NoteKind::AuxRoute.as_str().into(),
                value: "on".into(),
                choices: vec!["on".into(), "off".into()],
                help: "whether diverted aux calls announce themselves".into(),
            })
        })
        .collect()
}

// ── plugin action parking ───────────────────────────────────────────
//
// Plugin automation (veles::send_outbound, veles::call_llm, tool and
// command registration) happens on the Tcl thread, which has no
// session or transcript of its own. Same parking pattern as AUX_USED:
// the bridge parks ready-made notes, `Agent::drain_notes` surfaces
// them at the next emission point. This is the enforcement half of
// "silent plugin automation is prohibited".
static PLUGIN_NOTES: std::sync::Mutex<Vec<EngineNote>> = std::sync::Mutex::new(Vec::new());

/// Park a plugin-action note. Bounded, same as `record_aux_route` — a
/// plugin in a loop must not grow this without limit.
pub fn record_plugin_note(summary: &str, detail: &str) {
    if let Ok(mut v) = PLUGIN_NOTES.lock() {
        if v.len() < 32 {
            let mut n = EngineNote::new(NoteKind::Plugin, summary.to_string());
            if !detail.is_empty() {
                n = n.with_detail(detail.to_string());
            }
            v.push(n);
        }
    }
}

/// Take everything parked since the last call.
pub fn take_plugin_notes() -> Vec<EngineNote> {
    PLUGIN_NOTES
        .lock()
        .map(|mut v| std::mem::take(&mut *v))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_strings_round_trip() {
        for k in NoteKind::all() {
            assert_eq!(
                NoteKind::parse(k.as_str()),
                Some(*k),
                "{} must round-trip",
                k.as_str()
            );
        }
        assert_eq!(NoteKind::parse("nonsense"), None);
    }

    #[test]
    fn every_kind_has_a_distinct_key_and_glyph() {
        let mut keys: Vec<&str> = NoteKind::all().iter().map(|k| k.as_str()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "note kind keys must be unique");

        let mut glyphs: Vec<&str> = NoteKind::all().iter().map(|k| k.glyph()).collect();
        glyphs.sort_unstable();
        glyphs.dedup();
        assert_eq!(glyphs.len(), n, "note kind glyphs must be distinguishable");
    }

    #[test]
    fn line_is_one_line() {
        let n = EngineNote::new(NoteKind::Compaction, "compacted 86k→1k chars")
            .with_detail("kept 12 recent messages\narchived 340");
        assert!(!n.line().contains('\n'), "the summary line must not wrap");
        assert!(n.line().contains("compacted 86k→1k chars"));
        // detail may be multi-line; it is shown on demand.
        assert!(n.detail.contains('\n'));
    }

    #[test]
    fn on_off_toggle_names_the_mechanism() {
        let t = NoteToggle::on_off(NoteKind::Guard, true, "stop runaway tool loops");
        assert_eq!(t.key, "guard");
        assert_eq!(t.value, "on");
        assert_eq!(t.choices, vec!["on".to_string(), "off".to_string()]);
    }

    #[test]
    fn notes_serialize_compactly() {
        // Surfaces ship these over SSE; an empty detail/toggle must not
        // bloat every frame.
        let n = EngineNote::new(NoteKind::Guard, "halted after 5 identical calls");
        let j = serde_json::to_string(&n).unwrap();
        assert!(!j.contains("detail"), "empty detail is omitted: {j}");
        assert!(!j.contains("toggle"), "absent toggle is omitted: {j}");
        assert!(j.contains("\"kind\":\"guard\""), "{j}");
    }

    /// Every surface must render a note. This is the guarantee the
    /// StreamEvent design buys: the compiler already refuses to build a
    /// surface that ignores the variant, and this checks the two text
    /// printers actually emit the content rather than swallowing it.
    #[test]
    fn text_surfaces_render_the_note() {
        let n = EngineNote::new(NoteKind::Guard, "turn halted — iteration cap reached")
            .with_detail("raise the cap if the work needs more rounds")
            .with_toggle(NoteToggle {
                key: "agent.guards.max_iterations".into(),
                value: "500".into(),
                choices: vec![],
                help: "tool rounds per turn".into(),
            });

        // The NDJSON surface carries the whole thing, machine-readable.
        let line = crate::providers::json_event(&crate::providers::StreamEvent::Note(n.clone()));
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v["event"], "note");
        assert_eq!(v["kind"], "guard");
        assert_eq!(v["summary"], "turn halted — iteration cap reached");
        assert_eq!(v["toggle"]["key"], "agent.guards.max_iterations");
        assert_eq!(v["toggle"]["value"], "500");

        // …and the human line leads with the mechanism, not the detail.
        assert!(n.line().starts_with(NoteKind::Guard.glyph()));
        assert!(n.line().contains("iteration cap"));
    }

    #[test]
    fn catalogue_covers_every_note_kind() {
        // A mechanism that can emit a note but is missing from the
        // catalogue is one you can see fire and cannot turn off.
        for k in NoteKind::all() {
            assert!(
                catalogue().iter().any(|m| m.kind == *k),
                "{} emits notes but is not in the mechanism catalogue",
                k.as_str()
            );
        }
        assert_eq!(catalogue().len(), NoteKind::all().len());
    }

    #[test]
    fn listing_shows_defaults_and_overrides() {
        let listing = render_catalogue(
            &[("compaction".into(), "off".into())],
            &crate::config::MechanismsSection::default(),
        );
        assert!(
            listing.contains("compaction") && listing.contains("(session)"),
            "the session override is named as one: {listing}"
        );

        // All three layers are legible, and the connector layer says
        // what it actually does — trims that surface's copy, keeps the
        // transcript. "why am I not seeing guard notes" has to be
        // answerable from this listing.
        let mut m = crate::config::MechanismsSection {
            silenced: vec!["cache".into()],
            ..Default::default()
        };
        m.platforms.insert(
            "irc".into(),
            crate::config::PlatformNotes {
                verbosity: Some("quiet".into()),
                silenced: vec![],
            },
        );
        let layered = render_catalogue(&[], &m);
        assert!(
            layered.contains("off (config)"),
            "a globally silenced mechanism says so: {layered}"
        );
        assert!(
            layered.contains("on irc: verbosity quiet"),
            "and the connector layer is visible: {layered}"
        );
        assert!(
            layered.contains("the transcript keeps them"),
            "…including what it does NOT do: {layered}"
        );
        // A platform with nothing to say is not listed — but a CONNECTOR
        // always has something to say now: an unconfigured one silences
        // the bookkeeping kinds by default, and this listing is where an
        // operator finds that out. Inverted 2026-09-10 together with the
        // default itself; "no noise by default" used to be true only
        // because nothing was happening.
        let plain = render_catalogue(&[], &crate::config::MechanismsSection::default());
        assert!(
            plain.contains(
                "on irc: silenced by default: guard memory injection cache plugin approval"
            ),
            "the connector default is in force and must be legible: {plain}"
        );
        assert!(
            plain.contains("on telegram: silenced by default:"),
            "both connectors, not only the one that complained: {plain}"
        );
        assert!(
            !plain.contains("on desktop:"),
            "a surface that is not a connector has nothing to report: {plain}"
        );
        // The escape hatch is named in the same breath: a default whose
        // off switch nobody can find is a surprise, not a default.
        assert!(
            plain.contains("verbosity = \"normal\" here restores them"),
            "{plain}"
        );

        // An explicit profile displaces the default — INCLUDING "normal",
        // which is spelled exactly like the fallback and means somebody
        // looked at it and chose every kind.
        let mut normal = crate::config::MechanismsSection::default();
        normal.platforms.insert(
            "irc".into(),
            crate::config::PlatformNotes {
                verbosity: Some("normal".into()),
                silenced: vec![],
            },
        );
        let said = render_catalogue(&[], &normal);
        assert!(
            !said.contains("on irc:"),
            "explicit normal with nothing silenced has nothing to report: {said}"
        );
        assert!(
            said.contains("on telegram: silenced by default:"),
            "and it displaces the default on that platform only: {said}"
        );
        // Every mechanism is listed, not just the overridden one — the
        // point is discovering what CAN happen, not only what did.
        for k in NoteKind::all() {
            assert!(
                listing.contains(k.as_str()),
                "{} missing from the listing:\n{listing}",
                k.as_str()
            );
        }
        // CHURN: the state column used to read "off (session override)"
        // and "config default". The second said nothing about what the
        // default WAS, which is the question people arrive with, so the
        // states are now "off (session)" / "off (config)" / "on".
        assert!(listing.contains("off (session)"), "{listing}");
        assert!(
            listing.contains(" on "),
            "an un-silenced mechanism reads as on: {listing}"
        );
    }

    #[test]
    fn meta_keys_are_session_scoped() {
        let a = meta_key("sess-a", "guard");
        let b = meta_key("sess-b", "guard");
        assert_ne!(a, b, "one session's override must not leak into another");
        assert!(a.starts_with(META_PREFIX));
    }
    /// Aux diversions are parked process-globally (bridge.rs and
    /// titles.rs have no session or event sink) and drained by the
    /// agent. Taking must CLEAR, or every later turn re-reports the
    /// same diversion forever.
    #[test]
    fn aux_routes_are_parked_and_taken_once() {
        // AUX_USED is process-global, so two tests touching it in
        // parallel would interleave. Serialize on the shared test lock,
        // then start from a known state.
        let _guard = crate::testenv::lock();
        let _ = take_aux_notes();

        record_aux_route("session titling", "cheapo", "flash-1");
        record_aux_route("a plugin's LLM call", "cheapo", "flash-2");

        let notes = take_aux_notes();
        assert_eq!(notes.len(), 2, "both diversions become notes");
        assert!(matches!(notes[0].kind, NoteKind::AuxRoute));
        assert!(
            notes[0].summary.contains("session titling")
                && notes[0].summary.contains("cheapo/flash-1"),
            "the summary names the purpose and the route: {}",
            notes[0].summary
        );
        assert!(
            notes[0].toggle.is_some(),
            "a mechanism with a knob must offer it"
        );

        assert!(
            take_aux_notes().is_empty(),
            "taking clears — otherwise the same diversion is reported every turn"
        );
    }

    #[test]
    fn parked_aux_routes_are_bounded() {
        let _guard = crate::testenv::lock();
        let _ = take_aux_notes();
        // A plugin calling call_llm in a loop must not grow this without
        // limit while no turn is draining it.
        for i in 0..200 {
            record_aux_route("loop", "p", &format!("m{i}"));
        }
        let n = take_aux_notes().len();
        assert!(n <= 32, "parked aux routes must be capped, got {n}");
    }

    /// A cache miss note must NAME the block that changed — "cache miss"
    /// alone is exactly as useless as the silence it replaces.
    #[test]
    fn the_cache_note_names_the_block_that_changed() {
        let prev = vec![("MEMORY (your notes)".to_string(), 45_000)];
        let curr = vec![("MEMORY (your notes)".to_string(), 47_000)];
        let diff = prefix_block_diff(&prev, &curr).expect("something changed");
        assert!(
            diff.contains("MEMORY (your notes)")
                && diff.contains("45000")
                && diff.contains("47000"),
            "the diff names the block and both sizes: {diff}"
        );

        // A block that appears mid-session is named too.
        let curr2 = vec![
            ("MEMORY (your notes)".to_string(), 45_000),
            ("SKILLS".to_string(), 2_600),
        ];
        let diff2 = prefix_block_diff(&prev, &curr2).expect("something changed");
        assert!(
            diff2.contains("SKILLS") && diff2.contains("appeared"),
            "{diff2}"
        );

        // Identical blocks → no block-level explanation to offer.
        assert!(
            prefix_block_diff(&prev, &prev).is_none(),
            "nothing changed means the miss is not the blocks' fault"
        );
    }

    /// The half `prefix_block_diff` cannot see: with the fixed prefix
    /// unchanged, a miss is about HISTORY — and the useful answer names
    /// the row whose shape changed, not "history rewrite or eviction".
    /// The rewritten row is the one thing worth naming, because the
    /// engine caps tool output before it enters history and every later
    /// round re-sends it.
    #[test]
    fn the_history_tail_diff_names_the_rewritten_row() {
        let fp = history_tail_fingerprint;
        // Absolute row numbers, as history_tail_rows produces them.
        let rows = |v: &[(usize, &str, &str, usize)]| {
            v.iter()
                .map(|(i, r, n, c)| (*i, r.to_string(), n.to_string(), *c))
                .collect::<Vec<_>>()
        };

        // An APPEND is not a rewrite: the new tail is simply what the
        // cache could not have covered yet.
        let prev = fp(&rows(&[(0, "user", "", 10), (1, "assistant", "", 40)]));
        let grown = fp(&rows(&[
            (0, "user", "", 10),
            (1, "assistant", "", 40),
            (2, "tool", "terminal", 30_000),
        ]));
        let d = history_tail_diff(&prev, &grown).expect("it names the append");
        assert!(d.contains("grew by 1"), "{d}");
        assert!(
            !d.contains("changed shape"),
            "an append must not be reported as a rewrite: {d}"
        );

        // A REWRITE — the row an earlier round already sent changed
        // length (a re-cap, a re-render, a compaction). This is the shape
        // that reported itself as "provider-side eviction".
        let rewritten = fp(&rows(&[
            (0, "user", "", 10),
            (1, "assistant", "", 40),
            (2, "tool", "terminal", 12_000),
        ]));
        let d2 = history_tail_diff(&grown, &rewritten).expect("it names the rewrite");
        assert!(d2.contains("changed shape"), "{d2}");
        assert!(
            d2.contains("tool/terminal") && d2.contains("30000") && d2.contains("12000"),
            "the note names the row, its tool, and both sizes: {d2}"
        );
        assert!(d2.contains("history row 3"), "named by absolute row: {d2}");

        // THE SLIDING WINDOW. Once history grows past the 32-row window,
        // the same row sits at a different OFFSET in the newer window.
        // Comparing two windows positionally would report that shift as a
        // rewrite; absolute numbers must keep them aligned, so an append
        // is still an append and a rewrite is still found.
        let w1: Vec<_> = (0..32).map(|i| (i, format!("assistant:{i}"))).collect();
        let w2: Vec<_> = (8..40)
            .map(|i| {
                // Row 20 rewritten on its way into the newer window.
                if i == 20 {
                    (i, "assistant:9999".to_string())
                } else {
                    (i, format!("assistant:{i}"))
                }
            })
            .collect();
        let d3 = history_tail_diff(&w1, &w2).expect("the rewrite inside the shift is found");
        assert!(
            d3.contains("history row 21") && d3.contains("changed shape"),
            "the window shifted by 8 rows and row 20 (1-based 21) was still \
             identified, not the shift itself: {d3}"
        );

        // Identical tails → history is not the suspect either, so the
        // caller can honestly say the provider evicted.
        assert!(
            history_tail_diff(&grown, &grown).is_none(),
            "same bytes means neither prefix nor history explains it"
        );

        // A history that SHRANK (a compaction): the request the cache saw
        // is no longer a prefix of this one.
        let shorter = fp(&rows(&[(0, "user", "", 10)]));
        let d4 = history_tail_diff(&grown, &shorter).expect("it names the loss");
        assert!(d4.contains("lost 2 row"), "{d4}");
    }

    /// The Cache kind rides the same serialization contract as every
    /// other mechanism: surfaces ship these over the wire and filter on
    /// the kind string.
    #[test]
    fn the_cache_kind_round_trips_and_renders() {
        assert_eq!(NoteKind::parse("cache"), Some(NoteKind::Cache));
        let n = EngineNote::new(NoteKind::Cache, "cache miss: MEMORY grew 45k→47k chars");
        let j = serde_json::to_string(&n).unwrap();
        assert!(j.contains("\"kind\":\"cache\""), "{j}");
        assert!(n.line().starts_with(NoteKind::Cache.glyph()));
    }
}
