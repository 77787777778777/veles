//! `clarify` — the agent asks the user a question and waits.
//!
//! Rides the M11 ask channel, so it works on every surface that can
//! answer an approval: CLI prompt, TUI modal, telegram buttons, IRC
//! numbered reply, Tk dialog. Where nobody can answer (cron jobs, A2A
//! peers, `chat -q`, delegated children) it says so plainly instead of
//! hanging — hermes' version simply had no answer path there.
//!
//! The asker is looked up by session id at call time rather than held:
//! the desktop rebinds its asker to each request's SSE stream.

use crate::ask::{AskChoice, AskKind, AskQuestion, AskRequest};
use crate::tools::Tool;
use async_trait::async_trait;
use serde_json::{json, Value};

/// hermes caps the list at 4 (a 5th "other" is implicit in every UI).
pub const MAX_CHOICES: usize = 4;
/// claude-code's AskUserQuestion caps at 4 questions per call.
pub const MAX_QUESTIONS: usize = 4;

pub struct ClarifyTool {
    pub session_id: String,
    /// Seconds before an unanswered question gives up.
    pub timeout_secs: u64,
}

#[async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str {
        "clarify"
    }
    fn description(&self) -> &str {
        "Ask the user a question and wait for their answer. Use it when a choice is the \
user's to make and guessing wrong would waste real work — not for questions you can \
answer by reading the code. Offer up to 4 concrete choices when the options are known; \
omit them for an open question. Several related questions go in ONE call, as \
`questions`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "The question, one sentence"},
                "choices": {
                    "type": "array",
                    "description": "Up to 4 concrete options",
                    "items": {"type": "string"},
                    "maxItems": MAX_CHOICES
                },
                "multi_select": {
                    "type": "boolean",
                    "description": "Allow several choices (default false)"
                },
                "questions": {
                    "type": "array",
                    "description": "Up to 4, answered together in one dialog",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {"type": "string"},
                            "choices": {
                                "type": "array",
                                "items": {"type": "string"},
                                "maxItems": MAX_CHOICES
                            },
                            "multi_select": {"type": "boolean"}
                        },
                        "required": ["question"]
                    },
                    "maxItems": MAX_QUESTIONS
                }
            }
        })
    }

    async fn call(&self, args: Value) -> String {
        // RM3 #3: the tool tolerates the question shapes models
        // actually send, at the tool boundary — the agent loop's repair
        // ladder only runs on ONE of the doors into this tool
        // (tool_call envelopes and the CLI skip it). See
        // normalize_question for the shapes.
        let (question, payload_choices, _held_back) = match normalize_question(&args) {
            Ok(t) => t,
            Err(e) => return e,
        };
        let choices = parse_choices(args.get("choices").or(payload_choices.as_ref()));
        if choices.len() > MAX_CHOICES {
            return format!(
                "error: \"{question}\" has {} choices — at most {MAX_CHOICES} per question",
                choices.len()
            );
        }
        let multi = args
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Tier 3 (claude-code parity): several questions in ONE ask.
        // The engine's AskRequest grew `questions` for exactly this;
        // a first-class `questions` array builds the real thing, and
        // the RM3 tolerated shapes below CONVERT into it rather than
        // collapsing to the first question — the standing directive
        // from the 2026-09-15 alias incident is CONVERT, never
        // "ask one and hold the rest".
        // Every question in the payload is asked or the call is refused:
        // nothing is dropped and nothing is "held back".
        let questions = match parse_questions(&args, &question, &choices, multi) {
            Ok(q) => q,
            Err(e) => return e,
        };
        if questions.len() > MAX_QUESTIONS {
            return format!(
                "error: at most {MAX_QUESTIONS} questions in one call — split the rest \
into separate clarify calls"
            );
        }

        let mut req = AskRequest::multi_question(&self.session_id, AskKind::Clarify, questions)
            .with_timeout(self.timeout_secs);
        // Single-question shape: keep AskRequest::new's exact
        // behaviour (multi on the request itself).
        if req.questions.len() <= 1 {
            req.multi = multi;
        }
        // Relayed to EVERY attached surface (desktop window + chat
        // platform on the same session id): first answer wins, the
        // others' prompts close. `None` = nobody interactive attached —
        // the honest no-human answer, never a silently-wrong one
        // (PENDING 2026-09-14: this used to read a ONE-slot registry
        // the desktop's turn cleanup emptied, so `clarify` answered
        // itself "no user" while the owner was staring at the screen).
        let asked = req.clone();
        let Some(answer) = crate::ask::ask_session(&self.session_id, req).await else {
            return no_human();
        };
        if answer.timed_out {
            return "the user did not answer in time — proceed with your best judgment and \
say what you assumed"
                .into();
        }
        describe_answer(&asked, &answer)
    }
}

/// The tool result, in the model's own words: each question by its
/// text and each pick by its LABEL. `AskAnswer::describe` renders keys
/// ("q1: c1; q2: c1, c2") — right for an audit row, useless to a model
/// that never wrote those keys. A question left blank says so.
fn describe_answer(req: &AskRequest, a: &crate::ask::AskAnswer) -> String {
    let questions = req.all_questions();
    let one = |q: &AskQuestion, a: &crate::ask::AskAnswer| -> Option<String> {
        let mut parts: Vec<String> = a
            .selected
            .iter()
            .map(|k| {
                q.choices
                    .iter()
                    .find(|c| &c.key == k)
                    .map(|c| c.label.clone())
                    .unwrap_or_else(|| k.clone())
            })
            .collect();
        if let Some(t) = a
            .free_text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            parts.push(t.to_string());
        }
        (!parts.is_empty()).then(|| parts.join(", "))
    };
    // Esc / closing the dialog answers the approval verb `deny`, which
    // is no choice of a clarify question: the human dismissed it.
    let dismissed = a.per_question.is_empty()
        && a.free_text.is_none()
        && a.selected == [crate::ask::CHOICE_DENY]
        && !questions
            .iter()
            .any(|q| q.choices.iter().any(|c| c.key == crate::ask::CHOICE_DENY));
    if dismissed {
        return "the user dismissed the question without answering — proceed with your \
best judgment and say what you assumed"
            .into();
    }
    if questions.len() <= 1 {
        let q = &questions[0];
        return match one(q, a) {
            Some(t) => format!("the user answered: {t}"),
            None => "the user answered: (empty answer)".into(),
        };
    }
    let mut out = String::from("the user answered:");
    for (i, q) in questions.iter().enumerate() {
        // An answer without per-question entries (an older surface)
        // mirrors the FIRST question, which is what its top level means.
        let qa = match a.per_question.get(&q.key) {
            Some(x) => Some(x),
            None if a.per_question.is_empty() && i == 0 => Some(a),
            None => None,
        };
        let text = qa
            .and_then(|x| one(q, x))
            .unwrap_or_else(|| "(no answer)".into());
        out.push_str(&format!("\n{}. {} → {text}", i + 1, q.prompt));
    }
    out
}

/// Build the question list for the ask. First-class `questions` array
/// → one AskQuestion per entry. Otherwise the single question (whose
/// text/choices may themselves come from an unwrapped array shape)
/// stands alone. `held_back` questions from a tolerated array shape
/// are PROMOTED into the multi-question ask when they parse (so a
/// stringified two-question payload becomes a real two-question
/// dialog instead of "asked the first, held the rest").
fn parse_questions(
    args: &Value,
    question: &str,
    choices: &[AskChoice],
    multi: bool,
) -> Result<Vec<AskQuestion>, String> {
    // The first-class array wins when present and non-empty.
    if let Some(Value::Array(items)) = args.get("questions") {
        if !items.is_empty() {
            return items.iter().enumerate().map(question_from_value).collect();
        }
    }
    // RM3 tolerated shapes: a whole array of question objects arrived
    // under question/questions — as a REAL array, or as the
    // stringified transport form (the JSON-encoded string the
    // incident actually carried). Promote every parseable entry —
    // this is the "convert, don't collapse" fix: the 2026-09-15
    // incident's two-question stringified payload asks BOTH
    // questions in one dialog, not the first with a "held back" note.
    for key in ["question", "questions"] {
        let mut v = args.get(key).cloned();
        if let Some(Value::String(sv)) = &v {
            if let Ok(inner) = serde_json::from_str::<Value>(sv) {
                if inner.is_array() {
                    v = Some(inner);
                }
            }
        }
        let Some(Value::Array(items)) = v else {
            continue;
        };
        if !items.is_empty() {
            return items.iter().enumerate().map(question_from_value).collect();
        }
    }
    Ok(vec![
        AskQuestion::new("q1", question, choices.to_vec()).with_multi(multi)
    ])
}

/// One array entry → AskQuestion, or the refusal naming what is wrong
/// with it. Nothing is dropped: a question the model sent and the human
/// never saw is worse than an error the model can fix.
///
/// claude-code's AskUserQuestion spells the fields `options` (objects
/// with label/description) and `multiSelect`; a model trained on it
/// sends those, so they are read alongside this schema's own names.
fn question_from_value((i, v): (usize, &Value)) -> Result<AskQuestion, String> {
    let n = i + 1;
    let Value::Object(o) = v else {
        return Err(format!(
            "error: questions[{n}] is not an object — send {{question, choices, multi_select}}"
        ));
    };
    let q = o
        .get("question")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if q.is_empty() {
        return Err(format!("error: questions[{n}] has no question text"));
    }
    let choices = parse_choices(o.get("choices").or_else(|| o.get("options")));
    if choices.len() > MAX_CHOICES {
        return Err(format!(
            "error: \"{q}\" has {} choices — at most {MAX_CHOICES} per question",
            choices.len()
        ));
    }
    let multi = o
        .get("multi_select")
        .or_else(|| o.get("multiSelect"))
        .or_else(|| o.get("multi"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(AskQuestion::new("", q, choices).with_multi(multi))
}

fn no_human() -> String {
    "no interactive user is available in this session — proceed with your best judgment \
and state the assumption you made"
        .to_string()
}

/// RM3 #3: pull (question, optional payload choices, held-back count)
/// out of the shapes models actually send. The field report:
/// `{"questions": "[{\"question\": …, \"choices\": […]}]"}` — plural
/// key, value a JSON-ENCODED STRING of an array of question objects —
/// answered with "error: question is required". Tolerated shapes:
///   * `question` as a plain string (the advertised schema — untouched)
///   * `questions` (plural) holding any of the below
///   * a JSON string that parses to an array of question objects
///   * a real array of question objects (first asked, rest counted)
///   * a JSON string that parses to a single question object
///
/// The array forms keep the first question's own `choices`; an explicit
/// top-level `choices` still wins (call() prefers it).
fn normalize_question(args: &Value) -> Result<(String, Option<Value>, usize), String> {
    let mut src = args
        .get("question")
        .or_else(|| args.get("questions"))
        .cloned();
    // A JSON-encoded string unwraps once: string → array/object.
    if let Some(Value::String(s)) = &src {
        if let Ok(inner) = serde_json::from_str::<Value>(s) {
            if inner.is_array() || inner.is_object() {
                src = Some(inner);
            }
        }
    }
    // TRANSPORT, not intent: a stringified payload that LOOKS like a
    // serialized ask (starts with `[`/`{`) but will not parse must be
    // REFUSED, never asked as prose. The incident: a stringified array
    // of two {question, choices} objects with a stray trailing brace
    // fell to the plain-string arm and the WHOLE serialized array —
    // braces, escapes and both questions — became the ask PROMPT the
    // human was shown. A question that is visibly a mangled payload
    // teaches the model to send one question as a plain string; a
    // human staring at `[{\"question\": \"…\"}]` learns only that the
    // agent is broken.
    if let Some(Value::String(s)) = &src {
        let t = s.trim_start();
        if t.starts_with('[') || t.starts_with('{') {
            return Err(
                "error: the question arrived as a stringified JSON payload that does not \
                 parse — send the question as a plain string, or several as a `questions` \
                 array of {question, choices} objects"
                    .into(),
            );
        }
    }
    let mut held_back = 0usize;
    let question = match src {
        Some(Value::Array(items)) => {
            let first = items.iter().find(|it| it.get("question").is_some());
            let Some(first) = first else {
                return Err("error: question is required".into());
            };
            held_back = items
                .iter()
                .filter(|it| it.get("question").is_some())
                .count()
                - 1;
            let q = first.get("question").and_then(Value::as_str).unwrap_or("");
            let ch = first.get("choices").cloned();
            return finish(q, ch, held_back);
        }
        Some(Value::Object(o)) => {
            let q = o.get("question").and_then(Value::as_str).unwrap_or("");
            let ch = o.get("choices").cloned();
            return finish(q, ch, 0);
        }
        Some(Value::String(s)) => s,
        _ => return Err("error: question is required".into()),
    };
    finish(&question, None, held_back)
}

fn finish(
    q: &str,
    ch: Option<Value>,
    held: usize,
) -> Result<(String, Option<Value>, usize), String> {
    let q = q.trim();
    if q.is_empty() {
        return Err("error: question is empty".into());
    }
    Ok((q.to_string(), ch, held))
}

/// Choices may arrive as plain strings or as `{label|description|...}`
/// objects; models produce both shapes.
fn parse_choices(v: Option<&Value>) -> Vec<AskChoice> {
    let Some(Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let label = match it {
                Value::String(s) => s.trim().to_string(),
                Value::Object(_) => it
                    .get("label")
                    .or_else(|| it.get("description"))
                    .or_else(|| it.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                _ => String::new(),
            };
            if label.is_empty() {
                return None;
            }
            Some(AskChoice {
                key: format!("c{}", i + 1),
                label,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ask::{AskAnswer, Asker};
    use std::sync::Arc;

    struct Yes;
    #[async_trait]
    impl Asker for Yes {
        async fn ask(&self, req: AskRequest) -> AskAnswer {
            // Echo back what was actually offered, so the test sees it.
            AskAnswer::text(&format!(
                "{}|{}",
                req.prompt,
                req.choices
                    .iter()
                    .map(|c| c.label.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ))
        }
    }

    #[test]
    fn choices_accept_strings_and_objects() {
        let v = json!(["red", {"label": "green"}, {"description": "blue"}, 7, ""]);
        let c = parse_choices(Some(&v));
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].label, "red");
        assert_eq!(c[1].label, "green");
        assert_eq!(c[2].label, "blue");
        assert_eq!(c[0].key, "c1");
    }

    #[tokio::test]
    async fn asks_through_the_session_asker() {
        crate::ask::set_session_asker("clarify-test", Arc::new(Yes));
        let t = ClarifyTool {
            session_id: "clarify-test".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({"question": "which colour?", "choices": ["red", "green"]}))
            .await;
        assert!(out.contains("which colour?"), "{out}");
        assert!(out.contains("red,green"), "{out}");
        crate::ask::clear_session_asker("clarify-test");
    }

    #[tokio::test]
    async fn without_a_human_it_says_so_instead_of_hanging() {
        let t = ClarifyTool {
            session_id: "clarify-nobody".into(),
            timeout_secs: 5,
        };
        let out = t.call(json!({"question": "which colour?"})).await;
        assert!(out.contains("no interactive user"), "{out}");

        // A PolicyAsker (cron/A2A) is registered but not interactive.
        crate::ask::set_session_asker(
            "clarify-nobody",
            Arc::new(crate::ask::PolicyAsker { approve: false }),
        );
        let out = t.call(json!({"question": "which colour?"})).await;
        assert!(out.contains("no interactive user"), "{out}");
        crate::ask::clear_session_asker("clarify-nobody");
    }

    #[tokio::test]
    async fn empty_question_is_an_error() {
        let t = ClarifyTool {
            session_id: "clarify-empty".into(),
            timeout_secs: 5,
        };
        assert!(t.call(json!({})).await.starts_with("error:"));
        assert!(t
            .call(json!({"question": "  "}))
            .await
            .starts_with("error:"));
    }

    // ── RM3 #3: the payloads qwen-flash actually sent ───────────────
    //
    // USER: *'clarify {"questions": "[{\"question\": …, \"choices\":…]}"
    // ↳ error: question is required'*. Two independent mismatches in one
    // payload: the key (`questions`), and the value (a JSON-ENCODED
    // STRING of an array of question objects). The repair ladder's
    // rename only runs in the agent loop, so the TOOL itself tolerates
    // the shapes — clarify must work no matter which door it came
    // through (agent loop, tool_call envelope, CLI, plugin).

    /// THE field report, verbatim shape.
    #[tokio::test]
    async fn the_qwen_payload_asks_instead_of_erroring() {
        crate::ask::set_session_asker("clarify-qwen", Arc::new(Yes));
        let t = ClarifyTool {
            session_id: "clarify-qwen".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": "[{\"question\": \"How should the hover tooltip behave on the big text surfaces?\", \"choices\": [\"No floating tip on large panes\", \"Tip everywhere\"]}]"
            }))
            .await;
        assert!(out.contains("hover tooltip"), "asked: {out}");
        assert!(out.contains("No floating tip"), "choices arrived: {out}");
        assert!(out.contains("Tip everywhere"), "all choices: {out}");
        crate::ask::clear_session_asker("clarify-qwen");
    }

    /// Same shape as a real array (not stringified), and the
    /// stringified form landing under `question` after a legacy
    /// rename — both must CONVERT into a real multi-question ask:
    /// every question asked in ONE dialog (the 2026-09-26 parity
    /// change; the old behaviour asked the first and told the model
    /// the rest were "held back", which the 2026-09-15 alias incident
    /// called out as the lie it was).
    #[tokio::test]
    async fn array_and_stringified_forms_become_one_multi_question_ask() {
        use std::sync::Mutex;
        struct Capture(Mutex<Vec<AskRequest>>);
        #[async_trait]
        impl Asker for Capture {
            async fn ask(&self, req: AskRequest) -> AskAnswer {
                self.0.lock().unwrap().push(req.clone());
                AskAnswer::text(&format!(
                    "{}|{}",
                    req.prompt,
                    req.choices
                        .iter()
                        .map(|c| c.label.clone())
                        .collect::<Vec<_>>()
                        .join(",")
                ))
            }
        }
        let cap = std::sync::Arc::new(Capture(Mutex::new(Vec::new())));
        crate::ask::set_session_asker("clarify-unwrap", cap.clone());
        let t = ClarifyTool {
            session_id: "clarify-unwrap".into(),
            timeout_secs: 5,
        };
        let direct = t
            .call(json!({
                "questions": [
                    {"question": "first of several?", "choices": ["a", "b"]},
                    {"question": "second question?"}
                ]
            }))
            .await;
        assert!(direct.contains("first of several?"), "{direct}");
        assert!(
            !direct.contains("held back"),
            "nothing is held back any more: {direct}"
        );
        {
            let reqs = cap.0.lock().unwrap();
            assert_eq!(reqs.len(), 1, "ONE ask, not one per question");
            assert_eq!(reqs[0].questions.len(), 2, "both questions in it");
            assert_eq!(reqs[0].questions[1].prompt, "second question?");
        }

        let stringified = t
            .call(json!({
                "question": "[{\"question\": \"unwrapped how?\", \"choices\": [\"x\"]}]"
            }))
            .await;
        assert!(stringified.contains("unwrapped how?"), "{stringified}");
        assert!(stringified.contains("x"), "{stringified}");
        {
            let reqs = cap.0.lock().unwrap();
            assert_eq!(reqs.len(), 2);
            assert_eq!(
                reqs[1].questions.len(),
                1,
                "the stringified one-question form"
            );
        }

        // THE incident shape: stringified TWO-question payload — both
        // questions must be promoted into the one dialog, never
        // "asked the first, held the rest".
        let incident = t
            .call(json!({
                "questions": "[{\"question\": \"first?\", \"choices\": [\"a\"]}, {\"question\": \"second?\"}]"
            }))
            .await;
        assert!(incident.contains("the user answered"), "{incident}");
        let reqs = cap.0.lock().unwrap();
        assert_eq!(reqs.len(), 3);
        assert_eq!(
            reqs[2].questions.len(),
            2,
            "the stringified two-question form promotes BOTH"
        );
        crate::ask::clear_session_asker("clarify-unwrap");
    }

    /// The FIRST-CLASS shape: a native `questions` array is asked as one multi-question dialog
    /// with each question's own choices and selectivity intact.
    #[tokio::test]
    async fn the_native_questions_array_asks_every_question_at_once() {
        use std::sync::Mutex;
        struct Capture(Mutex<Vec<AskRequest>>);
        #[async_trait]
        impl Asker for Capture {
            async fn ask(&self, req: AskRequest) -> AskAnswer {
                self.0.lock().unwrap().push(req.clone());
                AskAnswer::text("ok")
            }
        }
        let cap = std::sync::Arc::new(Capture(Mutex::new(Vec::new())));
        crate::ask::set_session_asker("clarify-native", cap.clone());
        let t = ClarifyTool {
            session_id: "clarify-native".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": [
                    {"question": "which theme?", "choices": ["synthwave", "amber"]},
                    {"question": "art?", "choices": ["yes", "no"], "multi_select": true}
                ]
            }))
            .await;
        assert!(out.contains("the user answered"), "{out}");
        let reqs = cap.0.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let qs = &reqs[0].questions;
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].choices.len(), 2);
        assert!(!qs[0].multi, "single-select default");
        assert!(qs[1].multi, "per-question multi_select honoured");
        crate::ask::clear_session_asker("clarify-native");
    }

    /// The multi-question ANSWER round trip: the tool result names each
    /// question by its TEXT and each pick by its LABEL. It used to be
    /// "q1: c1; q2: c1, c2" — keys the model never wrote, to be mapped
    /// back by position — and a question a widget surface left blank
    /// simply vanished from the result.
    #[tokio::test]
    async fn the_answer_names_each_question_and_the_labels_chosen() {
        struct Picker;
        #[async_trait]
        impl Asker for Picker {
            async fn ask(&self, _req: AskRequest) -> AskAnswer {
                let mut per = std::collections::BTreeMap::new();
                per.insert("q1".to_string(), AskAnswer::choice("c1"));
                per.insert(
                    "q2".to_string(),
                    AskAnswer {
                        selected: vec!["c1".into(), "c2".into()],
                        ..Default::default()
                    },
                );
                per.insert("q3".to_string(), AskAnswer::text("Imperial Grid"));
                AskAnswer::from_per_question(per)
            }
        }
        crate::ask::set_session_asker("clarify-perq", std::sync::Arc::new(Picker));
        let t = ClarifyTool {
            session_id: "clarify-perq".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": [
                    {"question": "theme?", "choices": ["synthwave", "amber"]},
                    {"question": "art?", "choices": ["yes", "no"], "multi_select": true},
                    {"question": "name?"}
                ]
            }))
            .await;
        assert!(out.contains("theme? → synthwave"), "{out}");
        assert!(out.contains("art? → yes, no"), "{out}");
        assert!(out.contains("name? → Imperial Grid"), "{out}");
        assert!(!out.contains("c1"), "no bare keys: {out}");
        crate::ask::clear_session_asker("clarify-perq");

        // A question the surface left blank is SAID to be blank.
        struct Partial;
        #[async_trait]
        impl Asker for Partial {
            async fn ask(&self, _req: AskRequest) -> AskAnswer {
                let mut per = std::collections::BTreeMap::new();
                per.insert("q2".to_string(), AskAnswer::choice("c2"));
                AskAnswer::from_per_question(per)
            }
        }
        crate::ask::set_session_asker("clarify-partial", std::sync::Arc::new(Partial));
        let t = ClarifyTool {
            session_id: "clarify-partial".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": [
                    {"question": "theme?", "choices": ["synthwave", "amber"]},
                    {"question": "art?", "choices": ["yes", "no"]}
                ]
            }))
            .await;
        assert!(out.contains("theme? → (no answer)"), "{out}");
        assert!(out.contains("art? → no"), "{out}");
        crate::ask::clear_session_asker("clarify-partial");
    }

    /// One question: the label, not "c2". Esc/close on a clarify
    /// arrives as the `deny` key, which is no choice of the question:
    /// it is reported as a dismissal, not as an answer named "deny".
    #[tokio::test]
    async fn a_single_answer_names_the_label_and_a_dismissal_says_so() {
        struct Pick(&'static str);
        #[async_trait]
        impl Asker for Pick {
            async fn ask(&self, _req: AskRequest) -> AskAnswer {
                AskAnswer::choice(self.0)
            }
        }
        let t = ClarifyTool {
            session_id: "clarify-label".into(),
            timeout_secs: 5,
        };
        crate::ask::set_session_asker("clarify-label", Arc::new(Pick("c2")));
        let out = t
            .call(json!({"question": "which colour?", "choices": ["red", "green"]}))
            .await;
        assert_eq!(out, "the user answered: green");
        crate::ask::set_session_asker("clarify-label", Arc::new(Pick("deny")));
        let out = t
            .call(json!({"question": "which colour?", "choices": ["red", "green"]}))
            .await;
        assert!(out.contains("dismissed"), "{out}");
        assert!(!out.contains("answered: deny"), "{out}");
        crate::ask::clear_session_asker("clarify-label");
    }

    /// A question with more choices than the cap is REFUSED, like the
    /// top-level `choices` always was. It used to be dropped from the
    /// dialog without a word — the model believed it asked a question
    /// nobody saw.
    #[tokio::test]
    async fn a_question_with_too_many_choices_is_refused_not_dropped() {
        crate::ask::set_session_asker("clarify-cap", Arc::new(Yes));
        let t = ClarifyTool {
            session_id: "clarify-cap".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": [
                    {"question": "and?"},
                    {"question": "which?", "choices": ["a", "b", "c", "d", "e"]}
                ]
            }))
            .await;
        assert!(out.starts_with("error:") && out.contains("which?"), "{out}");
        crate::ask::clear_session_asker("clarify-cap");
    }

    /// claude-code's own AskUserQuestion names: `options` (objects with
    /// label/description) and `multiSelect`. A model trained on that
    /// tool sends them; reading only `choices`/`multi_select` turned
    /// every such question into free text, silently.
    #[tokio::test]
    async fn claude_codes_own_field_names_are_read() {
        use std::sync::Mutex;
        struct Capture(Mutex<Vec<AskRequest>>);
        #[async_trait]
        impl Asker for Capture {
            async fn ask(&self, req: AskRequest) -> AskAnswer {
                self.0.lock().unwrap().push(req.clone());
                AskAnswer::text("ok")
            }
        }
        let cap = std::sync::Arc::new(Capture(Mutex::new(Vec::new())));
        crate::ask::set_session_asker("clarify-cc", cap.clone());
        let t = ClarifyTool {
            session_id: "clarify-cc".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": [
                    {"question": "Which features?", "header": "Features", "multiSelect": true,
                     "options": [
                        {"label": "Search", "description": "full text"},
                        {"label": "Export", "description": "CSV"}
                     ]},
                    {"question": "Which db?", "header": "DB",
                     "options": [{"label": "sqlite"}, {"label": "postgres"}]}
                ]
            }))
            .await;
        assert!(out.contains("the user answered"), "{out}");
        let reqs = cap.0.lock().unwrap();
        let qs = &reqs[0].questions;
        assert_eq!(qs[0].choices.len(), 2, "{qs:?}");
        assert_eq!(qs[0].choices[0].label, "Search");
        assert!(qs[0].multi, "multiSelect honoured");
        assert_eq!(qs[1].choices.len(), 2);
        assert!(!qs[1].multi);
        drop(reqs);
        crate::ask::clear_session_asker("clarify-cc");
    }

    /// The OTHER half of the incident: the same stringified array with
    /// a stray trailing brace — unparseable. The old plain-string arm
    /// made the WHOLE serialized array the ask PROMPT, braces and
    /// escapes and both questions, and a human stared at
    /// `[{\"question\": …}]` wondering what was being asked. A payload
    /// that looks like JSON and will not parse is a TRANSPORT failure:
    /// refuse and teach the plain-string shape.
    #[tokio::test]
    async fn a_mangled_stringified_payload_is_refused_not_asked_as_prose() {
        crate::ask::set_session_asker("clarify-mangled", Arc::new(Yes));
        let t = ClarifyTool {
            session_id: "clarify-mangled".into(),
            timeout_secs: 5,
        };
        let out = t
            .call(json!({
                "questions": "[{\"question\": \"real question?\", \"choices\": [\"a\"]}]}"
            }))
            .await;
        assert!(
            out.starts_with("error:") && out.contains("does not parse"),
            "a transport refusal, not a question: {out}"
        );
        assert!(
            !out.contains("the user answered"),
            "nothing was asked: {out}"
        );
        crate::ask::clear_session_asker("clarify-mangled");
    }
}
