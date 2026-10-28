//! A question for the human, and the answer.
//!
//! Moved from `veles_core::ask` (2026-10-07), which re-exports them and
//! keeps everything with state: the pending registry, the legs, the
//! relay, the `Asker` trait. These are the shapes that cross a process
//! boundary.

use serde::{Deserialize, Serialize};

/// What is being asked. Frontends render approvals with more urgency
/// (and, on telegram, different buttons) than clarifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AskKind {
    Approval,
    Clarify,
    /// S16 failover decision: which route to try next / retry / stop.
    Failover,
    /// `secret_request`: the model needs a secret stored. The answer is
    /// only `set` or `decline` — the value goes into the vault by its
    /// own route (the desktop's masked field, `veles secret set`) and
    /// never through the ask. Rendered only on surfaces that can take
    /// input privately (`Asker::private_input` (veles-core)); never relayed to a
    /// chat, never sent to the owner's phone.
    Secret,
    /// A call homed on another node needs a ticket (PLAN-pool-round.md
    /// D3): the detail describes the exact call and ends with the
    /// request token. The answer is the TICKET, as free text — signed on
    /// a device that is not this node (the desktop signs with its own
    /// key; any surface takes one pasted, IRC and Telegram included,
    /// where a ticket typed as a plain message answers it), or the
    /// choice `decline`.
    Ticket,
    /// A bot challenge (a captcha, Cloudflare's "just a moment") stands
    /// between the agent and a page (PLAN-pool-round.md Part E). The
    /// ask carries a screenshot in `image`; a person answers with what
    /// to do on it — `click X Y`, `type TEXT`, `key NAME` as free text —
    /// or the choice `solved`, `give_up`, or `window` (open a real
    /// browser window on the node running it). One round per ask: the
    /// relay asks again with a fresh screenshot until the page is clear.
    Challenge,
}

/// Answer keys for a [`AskKind::Challenge`] ask.
pub const CHOICE_CHALLENGE_SOLVED: &str = "solved";
pub const CHOICE_CHALLENGE_GIVE_UP: &str = "give_up";
pub const CHOICE_CHALLENGE_WINDOW: &str = "window";

/// Answer key for declining a [`AskKind::Ticket`] ask.
pub const CHOICE_TICKET_DECLINE: &str = "decline";

/// Answer keys for a [`AskKind::Secret`] ask.
pub const CHOICE_SECRET_SET: &str = "set";
pub const CHOICE_SECRET_DECLINE: &str = "decline";

/// One selectable answer. `key` is the machine value, `label` the text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskChoice {
    pub key: String,
    pub label: String,
}

impl AskChoice {
    pub fn new(key: &str, label: &str) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
        }
    }
}

// Approval answer keys (hermes parity: once/session/always/deny).
pub const CHOICE_ONCE: &str = "once";
pub const CHOICE_SESSION: &str = "session";
pub const CHOICE_ALWAYS: &str = "always";
pub const CHOICE_DENY: &str = "deny";

/// Standard approval choices. `allow_permanent` is false when the aux
/// screen said DENY and the human is overriding it — a one-off override
/// should never silently become a permanent rule.
pub fn approval_choices(allow_permanent: bool) -> Vec<AskChoice> {
    let mut v = vec![AskChoice::new(CHOICE_ONCE, "allow once")];
    if allow_permanent {
        v.push(AskChoice::new(CHOICE_SESSION, "allow for this session"));
        v.push(AskChoice::new(CHOICE_ALWAYS, "always allow"));
    }
    v.push(AskChoice::new(CHOICE_DENY, "deny"));
    v
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskRequest {
    pub id: String,
    pub session_id: String,
    pub kind: AskKind,
    /// The question, one line where possible.
    pub prompt: String,
    /// Supporting detail (the command, the tool args, the aux verdict).
    #[serde(default)]
    pub detail: Option<String>,
    pub choices: Vec<AskChoice>,
    /// Clarify may accept several selections.
    #[serde(default)]
    pub multi: bool,
    /// The claude-code `AskUserQuestion` shape: several questions in
    /// ONE ask, each with its own choices and selectivity. Empty (the
    /// default, and the only shape approvals/failover ever use) means
    /// the prompt/choices/multi above ARE the single question — every
    /// pre-existing constructor and renderer of that shape stays
    /// valid untouched. Renderers must go through [`all_questions`],
    /// never read this raw.
    ///
    /// `prompt` still carries the FIRST question's text on a
    /// multi-question ask, so a consumer that predates the field (a
    /// plugin, an older desktop) degrades to showing one true
    /// question instead of nothing or a lie.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<AskQuestion>,
    pub timeout_secs: u64,
    /// A picture the question is about — a [`AskKind::Challenge`]'s
    /// screenshot, base64 PNG. Surfaces that can show it do; the others
    /// say where it can be seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// A question that may outlive the process asking it names its round
    /// here: a sync decision, asked from a phone that stops five minutes
    /// after its last use. The node that puts a relayed question to the
    /// owner keeps it open and keeps the answer under this key, and the
    /// asking node's next ask with the key (when it runs again) takes the
    /// answer instead of asking twice (veles_core::askrelay). `None`: the
    /// question lives and dies with its asker, as every ask did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_key: Option<String>,
}

/// One question inside a multi-question ask. Same shape as a
/// single-question `AskRequest` minus the plumbing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskQuestion {
    /// Machine key ("q1", "q2", …) — answers address their question
    /// by it (`AskAnswer::per_question`, the typed JSON reply, the
    /// telegram callback data).
    pub key: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<AskChoice>,
    #[serde(default)]
    pub multi: bool,
}

impl AskQuestion {
    pub fn new(key: &str, prompt: &str, choices: Vec<AskChoice>) -> Self {
        Self {
            key: key.to_string(),
            prompt: prompt.to_string(),
            choices,
            multi: false,
        }
    }
    pub fn with_multi(mut self, multi: bool) -> Self {
        self.multi = multi;
        self
    }
}

impl AskRequest {
    pub fn new(session_id: &str, kind: AskKind, prompt: &str, choices: Vec<AskChoice>) -> Self {
        Self {
            id: next_id(),
            session_id: session_id.to_string(),
            kind,
            prompt: prompt.to_string(),
            detail: None,
            choices,
            multi: false,
            questions: Vec::new(),
            timeout_secs: 300,
            image: None,
            relay_key: None,
        }
    }
    /// A multi-question ask (at least one question; the caller
    /// validates the cap — `clarify::MAX_QUESTIONS`). The request's
    /// own prompt/choices/multi are set from the FIRST question so a
    /// consumer that only knows the single-question shape still shows
    /// something true. Question keys are normalized to q1..qN when
    /// missing (answers address questions by key).
    pub fn multi_question(
        session_id: &str,
        kind: AskKind,
        mut questions: Vec<AskQuestion>,
    ) -> Self {
        for (i, q) in questions.iter_mut().enumerate() {
            if q.key.is_empty() {
                q.key = format!("q{}", i + 1);
            }
        }
        let mut req = Self::new(session_id, kind, "", Vec::new());
        if let Some(first) = questions.first() {
            req.prompt = first.prompt.clone();
            req.choices = first.choices.clone();
            req.multi = first.multi;
        }
        req.questions = questions;
        req
    }
    /// Every question in display order — the ONE accessor renderers
    /// and reply parsers may use. A single-question ask synthesizes
    /// its one question from the request's own fields.
    pub fn all_questions(&self) -> Vec<AskQuestion> {
        if self.questions.is_empty() {
            return vec![AskQuestion {
                key: "q1".to_string(),
                prompt: self.prompt.clone(),
                choices: self.choices.clone(),
                multi: self.multi,
            }];
        }
        self.questions.clone()
    }
    pub fn with_detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs.max(5);
        self
    }
    pub fn with_image(mut self, png_base64: impl Into<String>) -> Self {
        self.image = Some(png_base64.into());
        self
    }
    /// The picture as PNG bytes, for a surface that uploads it.
    pub fn image_png(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(self.image.as_deref()?.as_bytes())
            .ok()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AskAnswer {
    /// Chosen choice keys (empty for a pure free-text answer).
    #[serde(default)]
    pub selected: Vec<String>,
    /// Free text, when the frontend allows it (CLI "other" answers).
    #[serde(default)]
    pub free_text: Option<String>,
    /// Nobody answered in time.
    #[serde(default)]
    pub timed_out: bool,
    /// Per-question answers when the ask carried several questions.
    /// Keyed by the question's `key` ("q1", …); each value is that
    /// question's own selected/text answer. The top-level
    /// selected/free_text fields mirror the FIRST question's answer
    /// so consumers that predate multi-question asks (the approval
    /// gate, failover, audit renderers) read something true without
    /// knowing the shape exists.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub per_question: std::collections::BTreeMap<String, AskAnswer>,
}

impl AskAnswer {
    pub fn choice(key: &str) -> Self {
        Self {
            selected: vec![key.to_string()],
            ..Default::default()
        }
    }
    pub fn text(t: &str) -> Self {
        Self {
            free_text: Some(t.to_string()),
            ..Default::default()
        }
    }
    pub fn timeout() -> Self {
        Self {
            timed_out: true,
            ..Default::default()
        }
    }
    pub fn first(&self) -> Option<&str> {
        self.selected.first().map(|s| s.as_str())
    }
    /// Assemble a whole multi-question answer. `answers` must be
    /// non-empty and keyed by question key; the first question's
    /// answer (the map's first entry — q1..q4 sort first) is mirrored
    /// into the top-level fields.
    pub fn from_per_question(answers: std::collections::BTreeMap<String, AskAnswer>) -> Self {
        let mut out = Self::default();
        if let Some(a) = answers.values().next() {
            out.selected = a.selected.clone();
            out.free_text = a.free_text.clone();
        }
        out.per_question = answers;
        out
    }
    /// Human-readable rendering (tool results, audit rows).
    pub fn describe(&self) -> String {
        if self.timed_out {
            return "(no answer — timed out)".into();
        }
        if !self.per_question.is_empty() {
            // Sorted by the map's key order (q1, q2, …).
            let parts: Vec<String> = self
                .per_question
                .iter()
                .map(|(k, a)| format!("{k}: {}", a.describe()))
                .collect();
            return parts.join("; ");
        }
        let mut parts: Vec<String> = self.selected.clone();
        if let Some(t) = &self.free_text {
            if !t.trim().is_empty() {
                parts.push(t.trim().to_string());
            }
        }
        if parts.is_empty() {
            "(empty answer)".into()
        } else {
            parts.join(", ")
        }
    }
}

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A fresh ask id. One counter per process: the gateway mints the ids of
/// its asks here (core's own `next_id` was removed when these types moved,
/// so there is exactly one sequence), and a plugin's asks are re-minted by
/// the gateway when they are raised, never trusted from the plugin.
pub fn next_id() -> String {
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("ask{n}-{:x}", t & 0xffff_ffff)
}
