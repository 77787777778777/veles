//! Argument repair for tool calls (PENDING 2026-08-29, "meet the model
//! where it was trained").
//!
//! The inviolable rule: a call whose arguments could not be parsed or
//! repaired NEVER dispatches with `{}`. The old loop did
//! `serde_json::from_str(args).unwrap_or(Value::Object(Default::default()))`
//! — a fenced code block, bare shell line or malformed JSON silently
//! executed a write tool with default fields. That path is gone; this
//! ladder replaces it:
//!
//!   1. exact JSON
//!   2. exact JSON after fence-stripping (```json … ``` and friends)
//!   3. JSON5-style repair (trailing commas, single quotes, unquoted
//!      keys, comments stripped)
//!   4. bare-string fallback — the string becomes the tool's primary
//!      argument (per-tool table, then the schema's first string prop)
//!   5. refusal: a teachable error naming the tool, WHAT was wrong, and
//!      the expected schema.
//!
//! Field-level coercion then fixes wrong-but-unambiguous types under
//! the tool's own schema, each coercion noted in the result so the
//! model can conform next call. Repair fixes TRANSPORT, never intent:
//! parseable calls with wrong VALUES pass through untouched.

use serde_json::Value;

/// How a call's arguments got to where they landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Exact,
    Fenced,
    Repaired,
    BareString,
    CoercedField,
}

/// Per-(surface, kind) counters so "this model keeps making bad calls"
/// is measurable, not anecdotal. Cheap atomics; /status reads them.
pub struct Metrics {
    pub exact: u64,
    pub fenced: u64,
    pub repaired: u64,
    pub bare: u64,
    pub coerced: u64,
    pub refused: u64,
    pub alias: u64,
}

const COUNTERS: usize = 7;
static METRICS: [std::sync::atomic::AtomicU64; COUNTERS] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

fn bump(idx: usize) {
    let _ = METRICS[idx].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Alias-resolution hit (recorded by the tool registry).
pub fn metrics_alias_hit() {
    bump(6);
}

/// Snapshot for /status.
pub fn metrics() -> Metrics {
    let l = |i: usize| METRICS[i].load(std::sync::atomic::Ordering::Relaxed);
    Metrics {
        exact: l(0),
        fenced: l(1),
        repaired: l(2),
        bare: l(3),
        coerced: l(4),
        refused: l(5),
        alias: l(6),
    }
}

/// Minimal edit distance (Levenshtein) for did-you-mean suggestions.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let ins = cur[j] + 1;
            let del = prev[j + 1] + 1;
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = ins.min(del).min(sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Strip a markdown code fence if one wraps the JSON.
fn strip_fences(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if !raw.starts_with("```") {
        return None;
    }
    let after = raw.trim_start_matches('`');
    let after = after.trim_start();
    let lang_removed = after
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("json") || w.eq_ignore_ascii_case("js"));
    let mut body = if lang_removed {
        after[after.find(char::is_whitespace)?..].to_string()
    } else {
        after.to_string()
    };
    body = body.trim().to_string();
    let body = body.strip_suffix("```")?.trim().to_string();
    Some(body)
}

/// JSON5-style repairs: trailing commas, single quotes, unquoted keys,
/// shell-comment noise at the ends. Deliberately conservative — it
/// rewrites only shapes JSON parsers uniformly reject.
fn json5_repair(raw: &str) -> Option<String> {
    let mut s = raw.trim().to_string();
    // A trailing `;` from JS habits.
    while s.ends_with(';') {
        s.pop();
    }
    s = s.trim().to_string();
    if s.is_empty() || (!s.starts_with('{') && !s.starts_with('[')) {
        return None;
    }
    // Strip // and # comment lines only at the START (fences already
    // handled); comments INSIDE are too risky to strip blindly.
    let mut s = s.to_string();
    // Trailing commas before a closing brace/bracket.
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ',' {
            // look ahead past whitespace for }
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    s = out;
    // Single-quoted keys and strings → double-quoted. A full lexer is
    // out of scope; handle the common `{'a': 'b'}` and unquoted-key
    // shapes with a conservative pass over the quoting.
    let mut converted = String::new();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            '\'' => {
                // scan to the matching quote, re-escaping as we go
                let mut j = i + 1;
                let mut inner = String::new();
                while j < b.len() && b[j] != '\'' {
                    if b[j] == '"' {
                        inner.push_str("\\\"");
                    } else {
                        inner.push(b[j]);
                    }
                    j += 1;
                }
                if j < b.len() {
                    converted.push('"');
                    converted.push_str(&inner);
                    converted.push('"');
                    i = j + 1;
                } else {
                    converted.push('"');
                    converted.push_str(&inner);
                    i = j;
                }
            }
            c => {
                converted.push(c);
                i += 1;
            }
        }
    }
    s = converted;
    // Unquoted keys: after a `{` or `,` (skipping whitespace), an
    // identifier run immediately followed by `:` gets quoted.
    let mut keyed = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let mut prev_significant = '{'; // start-of-object context
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            keyed.push(c);
            i += 1;
            continue;
        }
        if c == '{' || c == ',' {
            keyed.push(c);
            prev_significant = c;
            i += 1;
            continue;
        }
        if (prev_significant == '{' || prev_significant == ',') && c.is_alphabetic() {
            // read the identifier
            let mut j = i;
            let mut ident = String::new();
            while j < chars.len()
                && (chars[j].is_alphanumeric() || chars[j] == '_' || chars[j] == '-')
            {
                ident.push(chars[j]);
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                keyed.push('"');
                keyed.push_str(&ident);
                keyed.push('"');
                prev_significant = 'k';
                i = j; // leave the ':' to the normal loop
                continue;
            }
            keyed.push_str(&ident);
            prev_significant = 'x';
            i = j;
            continue;
        }
        keyed.push(c);
        prev_significant = 'x';
        i += 1;
    }
    s = keyed;
    Some(s)
}

/// The parse ladder. Returns the parsed arguments plus where they came
/// from, or `Err` with a teachable refusal.
pub fn parse_arguments(
    raw: &str,
    tool_name: &str,
    schema: &Value,
) -> Result<(Value, Origin), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bump(0);
        return Ok((Value::Object(Default::default()), Origin::Exact));
    }

    // 1. Exact.
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.is_object() {
            refuse_repeated_keys(trimmed, tool_name)?;
            bump(0);
            return Ok((v, Origin::Exact));
        }
        // A bare scalar is not an arguments object; fall through to the
        // bare-string path so `"ls -la"` still reaches terminal.
    }

    // 2. Fence-stripped.
    if let Some(inner) = strip_fences(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(&inner) {
            if v.is_object() {
                refuse_repeated_keys(&inner, tool_name)?;
                bump(1);
                return Ok((v, Origin::Fenced));
            }
        }
    }

    // 3. JSON5-style repair.
    if let Some(repaired) = json5_repair(trimmed) {
        if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
            if v.is_object() {
                refuse_repeated_keys(&repaired, tool_name)?;
                bump(2);
                return Ok((v, Origin::Repaired));
            }
        }
    }

    // 4. Bare-string fallback → the tool's primary argument.
    if let Some(primary) = primary_field(tool_name, schema) {
        let probe = trimmed.trim_matches('"').trim();
        // Only when it is genuinely NOT an attempted JSON object — a
        // mangled `{...}` must refuse rather than become a command.
        if !trimmed.starts_with('{') && !trimmed.starts_with('[') && !probe.is_empty() {
            bump(3);
            let mut obj = serde_json::Map::new();
            obj.insert(primary, Value::String(probe.to_string()));
            return Ok((Value::Object(obj), Origin::BareString));
        }
    }

    bump(5);
    Err(format!(
        "tool `{tool_name}` was NOT called: its arguments could not be parsed: {trimmed:?}\n\
         expected arguments (JSON schema): {schema}\n\
         send corrected arguments to retry — a failed parse never dispatches."
    ))
}

/// Refuse arguments in which one object sets a key twice with DIFFERENT
/// values. `serde_json::Value` keeps the last value without a word, so a
/// model that packs three batch invocations into one object
/// (`{"arguments": A, "arguments": B, "arguments": C, "name": …}`) runs
/// only C and hears "1/1 ran" — how two edits vanished while the `rm`
/// meant to follow them ran (2026-09-28, session d544b5). An equal
/// repeat loses nothing and passes.
fn refuse_repeated_keys(json: &str, tool_name: &str) -> Result<(), String> {
    let Some((key, times)) = repeated_key(json) else {
        return Ok(());
    };
    bump(5);
    let hint = if key == "arguments" || key == "name" {
        " For several calls in one batch, give each its own \
         {\"name\": …, \"arguments\": {…}} object in `invocations`."
    } else {
        ""
    };
    Err(format!(
        "tool `{tool_name}` was NOT called: one object in its arguments sets \"{key}\" \
         {times} times with different values. JSON keeps only the last, so the others \
         would have been dropped silently — nothing ran.{hint} Send each key once."
    ))
}

/// The first key an object in `json` repeats with a different value, and
/// how many times that object sets it. Parses like `serde_json::Value`
/// but looks at every insert instead of letting the last one win.
fn repeated_key(json: &str) -> Option<(String, usize)> {
    use serde::de::{DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};

    struct Seed<'a>(&'a mut Option<(String, usize)>);

    impl<'de> DeserializeSeed<'de> for Seed<'_> {
        type Value = Value;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
            d.deserialize_any(self)
        }
    }

    impl<'de> Visitor<'de> for Seed<'_> {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("any JSON value")
        }
        fn visit_bool<E>(self, b: bool) -> Result<Value, E> {
            Ok(Value::Bool(b))
        }
        fn visit_i64<E>(self, n: i64) -> Result<Value, E> {
            Ok(n.into())
        }
        fn visit_u64<E>(self, n: u64) -> Result<Value, E> {
            Ok(n.into())
        }
        fn visit_f64<E>(self, n: f64) -> Result<Value, E> {
            Ok(n.into())
        }
        fn visit_str<E>(self, s: &str) -> Result<Value, E> {
            Ok(Value::String(s.to_owned()))
        }
        fn visit_string<E>(self, s: String) -> Result<Value, E> {
            Ok(Value::String(s))
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut out = Vec::new();
            while let Some(v) = seq.next_element_seed(Seed(&mut *self.0))? {
                out.push(v);
            }
            Ok(Value::Array(out))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut out = serde_json::Map::new();
            let mut times: std::collections::HashMap<String, usize> = Default::default();
            let mut clash: Option<String> = None;
            while let Some(k) = map.next_key::<String>()? {
                let v = map.next_value_seed(Seed(&mut *self.0))?;
                *times.entry(k.clone()).or_default() += 1;
                if clash.is_none() && out.get(&k).is_some_and(|old| *old != v) {
                    clash = Some(k.clone());
                }
                out.insert(k, v);
            }
            if let (None, Some(k)) = (&*self.0, clash) {
                let n = times[&k];
                *self.0 = Some((k, n));
            }
            Ok(Value::Object(out))
        }
    }

    let mut found = None;
    let mut de = serde_json::Deserializer::from_str(json);
    Seed(&mut found).deserialize(&mut de).ok()?;
    found
}

/// The primary string argument for tools that take one obvious target.
/// Static table first, then the schema's first `type: string` property.
fn primary_field(tool_name: &str, schema: &Value) -> Option<String> {
    let known = match tool_name {
        "terminal" => "command",
        "read_file" => "path",
        "search_files" => "pattern",
        "vision_analyze" => "image_url",
        "open_preview" => "url",
        "session_search" => "query",
        "web_search" => "query",
        "web_extract" => "url",
        "text_to_speech" => "text",
        "memory" => "content",
        "delegate_task" => "goal",
        _ => return schema_first_string(schema),
    };
    // The static entry must still exist in the schema (a renamed tool
    // must not be answered from a stale table).
    if schema["properties"].get(known).is_some() {
        Some(known.to_string())
    } else {
        schema_first_string(schema)
    }
}

fn schema_first_string(schema: &Value) -> Option<String> {
    let props = schema["properties"].as_object()?;
    for (k, v) in props {
        if v["type"].as_str() == Some("string") {
            return Some(k.clone());
        }
    }
    None
}

/// Field-level coercion under the tool's own schema. Each change is
/// noted so the model can conform next call. Wrong-but-unambiguous
/// types only: numeric strings → numbers, "true"/"false" → booleans,
/// and JSON-object STRINGS → objects for object-typed fields.
/// Parameter names other harnesses trained models to emit, mapped to
/// the name veles' schema uses.
///
/// The list is deliberately short and boring. Every entry is a pair
/// where the two names mean the SAME thing in every harness that uses
/// them — `file_path` and `path` are not a judgement call. Anything
/// that would require guessing intent is left out; a wrong rename is
/// worse than a refusal, because the refusal teaches and the rename
/// lies.
///
/// veles' own names already match Claude's for the hard cases —
/// `old_string`/`new_string` on `patch` are identical — so this is
/// mostly the one mismatch that breaks the three most-used tools.
const PARAM_ALIASES: &[(&str, &str)] = &[
    // Claude Code's Read/Write/Edit; also Cursor, Cline.
    ("file_path", "path"),
    ("filepath", "path"),
    ("filename", "path"),
    // Claude Code's Bash background flag — pure transport, same field,
    // different spelling upstream. `run_in_background` is its current
    // spelling (Bash and Task both), with the same meaning in terminal
    // and delegate_task.
    ("is_background", "background"),
    ("run_in_background", "background"),
    // OpenAI's str_replace_editor family.
    ("old_str", "old_string"),
    ("new_str", "new_string"),
    ("file_text", "content"),
    // Shell tools.
    ("cmd", "command"),
    // RM3 once renamed clarify's pluralised `questions` key here. The
    // 2026-09-26 parity round made `questions` a NATIVE clarify
    // parameter (an array of question objects asked in one dialog), so
    // the rename would now collapse a valid multi-question payload to
    // its first question — the exact silent mislabel this table
    // refuses to make. The row is gone on purpose; do not re-add it.
    // delegate_task's Claude-Code `Task` shape. `prompt` is the task to
    // run = `goal`; `subagent_type` is which defined subagent = `agent`.
    // Schema-guarded, so a2a (which really takes `agent` but has no
    // `subagent_type`) and imagegen/cronjob (which really take `prompt`)
    // are untouched. `description` (Task's short label) is deliberately
    // NOT renamed: a 3-word label is not a `goal`, and mapping it would
    // hand the child a near-empty task — a silent mislabel, which is
    // worse than the refusal that teaches.
    ("prompt", "goal"),
    ("subagent_type", "agent"),
    // RM3: Cline/Roo's replace_in_file vocabulary — search/replace
    // mean exactly old_string/new_string, and Roo's target_path /
    // target_file mean path.
    ("search", "old_string"),
    ("replace", "new_string"),
    ("replace_text", "new_string"),
    ("target_path", "path"),
    ("target_file", "path"),
];

/// Rename known-equivalent parameter names to what THIS tool's schema
/// asks for.
///
/// Strictly schema-guarded: a key is renamed only when the name the
/// model used is absent from the schema AND the target is present in
/// it AND the target is not already set. Under those three conditions
/// the rename cannot destroy information or invent a field — and a
/// tool whose schema genuinely takes `file_path` keeps it untouched.
///
/// This is the "meet the model where it was trained" case that argument
/// repair could not reach: `{"file_path": "/x"}` is perfectly valid
/// JSON, so the transport ladder passes it through happily and the tool
/// then fails on a missing `path`. The model has no way to learn from
/// that except by burning a round — and a heavily-RL'd model will emit
/// the same name again next round anyway.
pub fn rename_param_aliases(args: &mut Value, schema: &Value, notes: &mut Vec<String>) {
    let Value::Object(map) = args else { return };
    let Some(props) = schema["properties"].as_object() else {
        return;
    };
    for (from, to) in PARAM_ALIASES {
        if props.contains_key(*from) {
            continue; // this tool really does take that name
        }
        if !props.contains_key(*to) || map.contains_key(*to) {
            continue; // nowhere to put it, or already supplied
        }
        if let Some(v) = map.remove(*from) {
            notes.push(format!(
                "renamed \"{from}\" → \"{to}\"{}",
                pair_note(from, to)
            ));
            map.insert((*to).to_string(), v);
        }
    }
}

/// The honest per-pair note. The old text hard-coded "(same field
/// here)" for every pair, which was a LIE for `prompt → goal` (a
/// Task prompt IS the goal, but the note is where the model learns
/// the target's vocabulary). A rename that claims more than it did is
/// worse than none.
fn pair_note(from: &str, to: &str) -> &'static str {
    match (from, to) {
        // Task's `prompt` is the task to run = the child's goal.
        ("prompt", "goal") => " (the task to run)",
        // Task's `subagent_type` names a defined subagent.
        ("subagent_type", "agent") => " (which subagent to run)",
        _ => " (same field here)",
    }
}

// ── alias SHAPE conversions (2026-09-20 round) ───────────────────────
//
// PENDING §"Tool aliases convert a NAME, not a SHAPE": `rename_param_aliases`
// renames a parameter and never reshapes the value, so an alias whose
// upstream vocabulary differs in SHAPE arrived verbatim and failed
// teaching nothing. A conversion may REDIRECT the target (the
// str_replace_editor `command` discriminator picks read_file,
// write_file or patch), REWRITE the arguments into the target's
// vocabulary, or REFUSE by name with the reason. Every conversion runs
// BEFORE the gates classify the call, so the tool that actually runs is
// the tool the gates judged — a `str_replace_editor{command:create}`
// that dispatched write_file while the ACL classified `patch` would be
// a gate bypass, not a convenience.

/// Typed names carrying a declared SHAPE conversion. Meta-tested: every
/// row in `default_aliases` must appear here or in `NAME_ONLY_ALIASES`,
/// so adding an alias row forces the author to say which kind it is.
pub const SHAPE_CONVERTED_ALIASES: &[&str] = &[
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "str_replace",
    "apply_patch",
    "insert_content",
    "code_interpreter",
    "execute",
    "glob",
    "bashoutput",
    "taskoutput",
    "killshell",
    "killbash",
    "taskstop",
];

/// Alias rows declared NAME-ONLY (shape identical to the target's), the
/// meta-test's other half. A row in neither list fails the gate.
pub const NAME_ONLY_ALIASES: &[&str] = &[
    "read",
    "write",
    "edit",
    "bash",
    "run_command",
    "web_fetch",
    "fetch_url",
    "websearch",
    "web_search_preview",
    "todo_write",
    "create_file",
    "view_file",
    "view",
    "open_file",
    "cat",
    "find_files",
    "ripgrep",
    "rg",
    "search",
    "run_terminal_cmd",
    "run_shell_command",
    "exec",
    "sh",
    "task",
    "dispatch_agent",
    "todoread",
    "exit_plan_mode",
    "get_plan",
    "current_plan",
    "read_plan",
    "view_plan",
    "plan_status",
    "grep",
];

/// What a shape conversion decided for one call.
#[derive(Debug)]
pub enum ShapeOutcome {
    /// Run this target with these args. The target MAY differ from the
    /// alias's static resolution (the discriminator picked another
    /// tool); the args are already in the target's vocabulary.
    Run {
        target: String,
        args: Value,
        notes: Vec<String>,
    },
    /// A teaching refusal: the alias cannot be honored honestly, and
    /// the text says what the real tool CAN express.
    Refuse(String),
    /// No conversion declared — a name-only alias, or the typed name is
    /// not an alias key. The caller proceeds with the normal ladder.
    None,
}

/// Convert one alias-shaped call. `args_raw` is the unparsed argument
/// string; conversions that need the discriminator parse it themselves
/// (permissively — a payload that will not parse falls through to the
/// normal ladder, whose refusal against the target's real schema
/// teaches more than ours could).
pub fn convert_alias_shape(name: &str, args_raw: &str) -> ShapeOutcome {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        // OpenAI's editor family: one tool, a `command` discriminator.
        "str_replace_editor" | "str_replace_based_edit_tool" | "str_replace" => {
            let Ok((mut args, _)) =
                parse_arguments(args_raw, &name, &serde_json::json!({"type": "object"}))
            else {
                return ShapeOutcome::None; // the ladder's refusal is the better teacher
            };
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // The vocabulary keys arrive under several upstream names;
            // take whichever is present.
            let take = |args: &mut Value, keys: &[&str]| -> Option<Value> {
                let Value::Object(map) = args else {
                    return None;
                };
                keys.iter().find_map(|k| map.remove(*k))
            };
            let path = take(
                &mut args,
                &[
                    "path",
                    "file_path",
                    "filepath",
                    "filename",
                    "target_path",
                    "target_file",
                ],
            );
            let path_str = path
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            match command.as_str() {
                "view" => ShapeOutcome::Run {
                    target: "read_file".into(),
                    args: serde_json::json!({ "path": path_str }),
                    notes: vec![format!(
                        "[{name} command=view → read_file] the editor's view is this harness's read_file"
                    )],
                },
                "create" => {
                    let content = take(&mut args, &["content", "file_text", "new_str", "new_string"]);
                    ShapeOutcome::Run {
                        target: "write_file".into(),
                        args: serde_json::json!({
                            "path": path_str,
                            "content": content.unwrap_or(Value::Null),
                        }),
                        notes: vec![format!(
                            "[{name} command=create → write_file] a create is a full-file write"
                        )],
                    }
                }
                "str_replace" => {
                    let old = take(&mut args, &["old_str", "old_string", "search"]);
                    let new = take(&mut args, &["new_str", "new_string", "replace", "replace_text"]);
                    ShapeOutcome::Run {
                        target: "patch".into(),
                        args: serde_json::json!({
                            "path": path_str,
                            "old_string": old.unwrap_or(Value::Null),
                            "new_string": new.unwrap_or(Value::Null),
                        }),
                        notes: vec![format!(
                            "[{name} command=str_replace → patch] the editor's str_replace is this harness's patch"
                        )],
                    }
                }
                "insert" => ShapeOutcome::Refuse(format!(
                    "error: `{name}` command=insert has no honest conversion — this harness's `patch` \
                     replaces a UNIQUE old_string with new_string. Give the line(s) the insert goes \
                     BETWEEN as old_string and the new content plus that context as new_string."
                )),
                other => ShapeOutcome::Refuse(format!(
                    "error: `{name}` command={other:?} is not one of view/create/str_replace/insert. \
                     This harness has the real tools directly: read_file, write_file, patch."
                )),
            }
        }
        // Codex's freeform patch format (*** Begin Patch …) is a text
        // protocol; `patch` speaks JSON old_string/new_string. No honest
        // conversion — teach instead of garbling.
        "apply_patch" => ShapeOutcome::Refuse(
            "error: `apply_patch`'s freeform patch format has no conversion here — `patch` takes \
             {path, old_string, new_string}: one unique old_string per call, replaced by \
             new_string. (For an INSERT, put the surrounding context in old_string and context + \
             new lines in new_string.)"
                .into(),
        ),
        // {path, line, content} — a line-indexed insert patch cannot
        // express (and a line NUMBER the model computed from a stale
        // read is exactly the drift patch's uniqueness check exists to
        // catch).
        "insert_content" => ShapeOutcome::Refuse(
            "error: `insert_content`'s {path, line, content} has no conversion here — `patch` \
             anchors on CONTENT, not line numbers: old_string = the line(s) the content goes \
             between, new_string = that context plus the new content."
                .into(),
        ),
        // Trained meaning is "run Python". execute_code here is Tcl
        // (policy: no Python) — letting `print(1)` die as `invalid
        // command name` teaches nothing about WHY.
        "code_interpreter" | "execute" => ShapeOutcome::Refuse(
            "error: this harness has no Python interpreter — `execute_code` evaluates Tcl \
             (tclsh), and `terminal` runs shell. `puts [expr {1+1}]` is the Tcl spelling of \
             print(1+1)."
                .into(),
        ),
        // A glob is a FILE-NAME pattern, not a regex: search_files' \
        // default target=content compiles it as ripgrep regex.
        "glob" => {
            let Ok((args, _)) =
                parse_arguments(args_raw, &name, &serde_json::json!({"type": "object"}))
            else {
                return ShapeOutcome::None;
            };
            let pattern = args
                .get("pattern")
                .and_then(Value::as_str)
                .or_else(|| args.get("path").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            if pattern.is_empty() {
                return ShapeOutcome::None; // let the ladder refuse on the real schema
            }
            // search_files target=files matches BASENAMES; a leading
            // **/ says "any directory", which basename mode means
            // already.
            let stripped = pattern.strip_prefix("**/").unwrap_or(&pattern).to_string();
            let dir = args.get("path").and_then(Value::as_str).map(str::to_string);
            let mut out = serde_json::json!({
                "pattern": stripped,
                "target": "files",
            });
            if let Some(d) = dir {
                out["path"] = serde_json::json!(d);
            }
            ShapeOutcome::Run {
                target: "search_files".into(),
                args: out,
                notes: vec![
                    "[glob → search_files target=files] a glob is a file-NAME pattern; \
                     content mode would compile it as a regex"
                        .to_string(),
                ],
            }
        }
        // Claude Code's background-task tools, onto `shell`:
        // BashOutput/TaskOutput read (TaskOutput BLOCKS by default, its
        // `block`), KillShell/KillBash/TaskStop stop. The id arrives as
        // bash_id / task_id / shell_id and often as a string; the timeout
        // is in milliseconds. `shell kill` goes through the shared task
        // registry, so a monitor or subagent id stops too.
        "bashoutput" | "taskoutput" | "killshell" | "killbash" | "taskstop" => {
            let Ok((args, _)) =
                parse_arguments(args_raw, &name, &serde_json::json!({"type": "object"}))
            else {
                return ShapeOutcome::None;
            };
            let id = ["task_id", "bash_id", "shell_id", "id"]
                .iter()
                .find_map(|k| args.get(*k))
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<u64>().ok()))
                });
            let Some(id) = id else {
                return ShapeOutcome::Refuse(format!(
                    "error: `{name}` needs the task id — the integer a background \
                     terminal/monitor/delegate call returned as task_id"
                ));
            };
            let (out, note) = if name.starts_with("kill") || name == "taskstop" {
                (
                    serde_json::json!({"action": "kill", "id": id}),
                    format!("[{name} → shell kill]"),
                )
            } else {
                let block = args
                    .get("block")
                    .and_then(Value::as_bool)
                    .unwrap_or(name == "taskoutput");
                let mut out = serde_json::json!({
                    "action": if block { "wait" } else { "output" },
                    "id": id,
                });
                if let Some(ms) = args.get("timeout").and_then(Value::as_u64) {
                    out["timeout"] = serde_json::json!(ms.div_ceil(1000).max(1));
                }
                let verb = if block { "wait" } else { "output" };
                (
                    out,
                    format!("[{name} → shell {verb}] timeout is seconds here"),
                )
            };
            ShapeOutcome::Run {
                target: "shell".into(),
                args: out,
                notes: vec![note],
            }
        }
        _ => ShapeOutcome::None,
    }
}

/// Claude-family `timeout` values arrive in MILLISECONDS; every veles
/// `timeout` field is SECONDS. A ms value silently clamps to the max —
/// a 600000 (10 min) becomes 3600s and the model never learns. A value
/// above the largest seconds field we ship (3600) is read as ms and
/// divided, with a note. Below that, it is a perfectly good seconds
/// count and passes through.
pub fn convert_timeout_units(args: &mut Value, notes: &mut Vec<String>) {
    const MAX_SECS: u64 = 3600;
    let Value::Object(map) = args else { return };
    let Some(v) = map.get_mut("timeout") else {
        return;
    };
    if let Some(n) = v.as_u64() {
        if n > MAX_SECS {
            let secs = (n / 1000).max(1);
            notes.push(format!(
                "timeout: {n} read as milliseconds → {secs}s (this field is seconds)"
            ));
            *v = serde_json::json!(secs);
        }
    }
}

pub fn coerce_args(args: &mut Value, schema: &Value, notes: &mut Vec<String>) {
    let Value::Object(map) = args else { return };
    let Some(props) = schema["properties"].as_object() else {
        return;
    };
    let before = notes.len();
    for (k, spec) in props {
        let Some(v) = map.get_mut(k) else { continue };
        let expect = spec["type"].as_str().unwrap_or("");
        match expect {
            // A string that is clearly "true"/"false" for a boolean field.
            "boolean" => {
                if let Value::String(s) = v {
                    if s == "true" || s == "false" {
                        notes.push(format!("coerced \"{k}\": string {s:?} → boolean"));
                        *v = Value::Bool(s == "true");
                    }
                }
            }
            // "42"/"3.5" for number/integer fields.
            "number" | "integer" => {
                if let Value::String(s) = v {
                    if expect == "integer" {
                        if let Ok(n) = s.trim().parse::<i64>() {
                            notes.push(format!("coerced \"{k}\": numeric string → integer"));
                            *v = Value::Number(n.into());
                        }
                    } else if let Ok(n) = s.trim().parse::<f64>() {
                        notes.push(format!("coerced \"{k}\": numeric string → number"));
                        *v = serde_json::Number::from_f64(n)
                            .map(Value::Number)
                            .unwrap_or(Value::Null);
                    }
                }
            }
            // A JSON blob handed over as a string for an object field.
            "object" => {
                if let Value::String(s) = v {
                    if let Ok(inner) = serde_json::from_str::<Value>(s) {
                        if inner.is_object() {
                            notes.push(format!("coerced \"{k}\": JSON string → object"));
                            *v = inner;
                        }
                    }
                }
            }
            // RM3: the same for ARRAY fields — qwen-flash sent choices
            // as a JSON-ENCODED STRING of an array; without this arm
            // it arrived as one giant string and every option welded
            // into one.
            "array" => {
                if let Value::String(s) = v {
                    if let Ok(inner) = serde_json::from_str::<Value>(s) {
                        if inner.is_array() {
                            notes.push(format!("coerced \"{k}\": JSON string → array"));
                            *v = inner;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // Count only actual coercions. (An earlier version bumped
    // unconditionally and undid it with a fetch_sub when nothing
    // changed — under concurrent sessions the decrement could land in
    // a different window than the increment, skewing /status.)
    if notes.len() > before {
        bump(4);
    }
}

/// RM3 #5: models expect consecutive file ops to reuse the previous
/// path when none is given — a pathless `patch` after a `read_file`
/// was refused with "path, old_string, new_string are required", which
/// taught nothing and burned a round. When the session has a file
/// focus (the last successfully touched file, see
/// harness::file_focus_*) and the call omits `path`, inject it and SAY
/// SO — the model must know what was assumed.
///
/// Scope: `patch` and `read_file` only. A pathless `write_file` would
/// INFER a full-file overwrite target — the one place inference turns
/// dangerous — so write_file keeps demanding an explicit path.
/// Schema-guarded like the renames: the tool must actually take a
/// `path`, and an explicit path is never overridden.
pub fn fill_implicit_path(
    tool: &str,
    focus: Option<&str>,
    schema: &Value,
    args: &mut Value,
    notes: &mut Vec<String>,
) {
    if tool != "patch" && tool != "read_file" {
        return;
    }
    let Some(p) = focus else { return };
    if p.is_empty() {
        return;
    }
    let Some(props) = schema["properties"].as_object() else {
        return;
    };
    if !props.contains_key("path") {
        return;
    }
    let Value::Object(map) = args else { return };
    if map
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
    {
        return;
    }
    notes.push(format!(
        "path inferred from the previous file call: {p:?} — pass \"path\" explicitly to target another file"
    ));
    map.insert("path".to_string(), Value::String(p.to_string()));
}

/// How deep an envelope may nest before it is refused instead of opened.
const MAX_ENVELOPE_DEPTH: usize = 4;

/// `{"name": …?, "arguments": …}` and nothing else: the shape of a
/// `tool_call` or a `batch` item, sent where a tool's own arguments go.
fn envelope_parts(v: &Value) -> Option<(Option<&str>, &Value)> {
    let map = v.as_object()?;
    let inner = map.get("arguments")?;
    if map.keys().any(|k| k != "name" && k != "arguments") {
        return None;
    }
    let name = match map.get("name") {
        None => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return None,
    };
    Some((name, inner))
}

/// Open a call that arrived wrapped in another call.
///
/// Some models send a tool's arguments inside the `{"name": …,
/// "arguments": …}` shape that `tool_call` and `batch` items use, often
/// with the inner arguments as a JSON STRING — `batch({"name":
/// "tool_call", "arguments": "{\"invocations\": […]}"})`,
/// `terminal({"name": "terminal", "arguments": "{\"command\": …}"})` —
/// or nest `tool_call` inside `tool_call`. qwen3.8-flash did all three
/// on IRC on 2026-10-11 (session 5de12a): five batches were refused
/// with "`invocations` must be an array", three terminals with
/// "'command' is required", and two `tool_call`s in `tool_call` were
/// denied the `mcp` capability, because the gate judged the inner name,
/// which was `tool_call` itself.
///
/// This fixes transport, not intent. An envelope opens only when it
/// names the tool that was called (an alias of it counts), or
/// `tool_call`, the dispatcher models wrap everything in, and only when
/// that tool's schema has no `arguments` field of its own. An envelope
/// naming ANOTHER tool is refused: running it would call a tool the
/// model did not call. It runs before every gate, so the gates judge
/// the opened call. A `tool_call` naming `tool_call` is flattened to
/// the call inside it, or refused when there is none.
pub fn unwrap_call_envelope(
    args: &mut Value,
    target: &str,
    schema: &Value,
    resolve: &dyn Fn(&str) -> String,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    if target == "tool_call" {
        return flatten_nested_tool_call(args, resolve, notes);
    }
    let props = schema["properties"].as_object();
    if props.is_some_and(|p| p.contains_key("arguments")) {
        return Ok(());
    }
    let mut opened = 0usize;
    while let Some((name, inner)) = envelope_parts(args) {
        if let Some(n) = name {
            let r = resolve(n);
            if r != target && r != "tool_call" {
                // The tool's own `name` field with a stray `arguments`
                // next to it: not ours to judge. The tool's own
                // validation answers.
                if props.is_some_and(|p| p.contains_key("name")) {
                    break;
                }
                bump(5);
                return Err(format!(
                    "tool `{target}` was NOT called: its arguments are a call to `{n}` \
                     ({{\"name\": …, \"arguments\": …}}). Call `{n}` itself, or send \
                     `{target}` its own arguments: {schema}"
                ));
            }
        }
        if opened == MAX_ENVELOPE_DEPTH {
            bump(5);
            return Err(format!(
                "tool `{target}` was NOT called: its arguments are {{\"name\", \"arguments\"}} \
                 envelopes nested more than {MAX_ENVELOPE_DEPTH} deep. Send `{target}` its own \
                 arguments: {schema}"
            ));
        }
        let unwrapped = match inner {
            Value::Object(_) => inner.clone(),
            Value::String(s) => parse_arguments(s, target, schema)?.0,
            // Not an arguments object at all; the tool's own
            // required-field error says what is missing.
            _ => break,
        };
        *args = unwrapped;
        opened += 1;
    }
    if opened > 0 {
        bump(4);
        notes.push(format!(
            "your arguments were wrapped in a {{\"name\", \"arguments\"}} call; ran `{target}` \
             with the arguments inside it — send them directly"
        ));
    }
    Ok(())
}

/// `tool_call({"name": "tool_call", "arguments": {"name": X, …}})` runs
/// X. Without this the gates judge the inner name `tool_call` as the
/// target: a guest is denied the `mcp` capability for a web search.
fn flatten_nested_tool_call(
    args: &mut Value,
    resolve: &dyn Fn(&str) -> String,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let mut flattened = 0usize;
    while args["name"].as_str().map(resolve).as_deref() == Some("tool_call") {
        let inner = match &args["arguments"] {
            Value::String(s) => serde_json::from_str::<Value>(s).ok(),
            v => Some(v.clone()),
        };
        let Some(inner) = inner.filter(|v| {
            flattened < MAX_ENVELOPE_DEPTH
                && envelope_parts(v).is_some_and(|(name, _)| name.is_some())
        }) else {
            bump(5);
            return Err(
                "tool `tool_call` was NOT called: it names `tool_call` itself, and the \
                 arguments inside are not a call. Name the deferred tool: \
                 {\"name\": \"web_search\", \"arguments\": {\"query\": \"…\"}}"
                    .to_string(),
            );
        };
        *args = inner;
        flattened += 1;
    }
    if flattened > 0 {
        bump(4);
        notes.push(format!(
            "`tool_call` was wrapped in `tool_call`; ran the call inside it (`{}`) — name the \
             deferred tool directly",
            args["name"].as_str().unwrap_or_default()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout": {"type": "integer"}
            }
        })
    }

    #[test]
    fn exact_json_parses() {
        let (v, o) = parse_arguments(r#"{"command":"ls"}"#, "terminal", &schema()).unwrap();
        assert_eq!(o, Origin::Exact);
        assert_eq!(v["command"], "ls");
    }

    #[test]
    fn empty_arguments_are_an_empty_object() {
        let (v, o) = parse_arguments("", "terminal", &schema()).unwrap();
        assert_eq!(o, Origin::Exact);
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    fn fenced_json_is_unwrapped() {
        let (v, o) = parse_arguments(
            "```json\n{ \"command\": \"ls\" }\n```",
            "terminal",
            &schema(),
        )
        .unwrap();
        assert_eq!(o, Origin::Fenced);
        assert_eq!(v["command"], "ls");
    }

    #[test]
    fn json5_trailing_commas_and_single_quotes_repair() {
        let (v, o) = parse_arguments("{'command': 'ls',}", "terminal", &schema()).unwrap();
        assert_eq!(o, Origin::Repaired);
        assert_eq!(v["command"], "ls");
    }

    /// The d544b5 batch: three invocations packed into ONE object share a
    /// single "name" and repeat "arguments". serde kept only the last (an
    /// `rm`), dropped the two edits before it, and the batch said "1/1
    /// ran". Refused now, naming the key; an equal repeat still passes.
    #[test]
    fn a_key_repeated_with_different_values_is_refused_not_silently_dropped() {
        let raw = r#"{"invocations": [{"arguments": {"path": "a", "old_string": "x", "new_string": "y"}, "arguments": {"path": "a", "old_string": "p", "new_string": "q"}, "arguments": {"command": "rm -f b"}, "name": "terminal"}]}"#;
        let err = parse_arguments(raw, "batch", &serde_json::json!({"type": "object"}))
            .expect_err("a batch that would silently lose two calls must not run");
        assert!(
            err.contains("\"arguments\" 3 times") && err.contains("nothing ran"),
            "{err}"
        );
        assert!(
            err.contains("invocations"),
            "teaches the batch shape: {err}"
        );
        assert!(
            parse_arguments(
                r#"{"command":"ls","command":"rm -rf x"}"#,
                "terminal",
                &schema()
            )
            .is_err(),
            "a plain call with a clash is refused too"
        );
        let (v, _) =
            parse_arguments(r#"{"command":"ls","command":"ls"}"#, "terminal", &schema()).unwrap();
        assert_eq!(v["command"], "ls", "an equal repeat loses nothing");
    }

    #[test]
    fn bare_string_becomes_the_primary_arg() {
        let (v, o) = parse_arguments("ls -la", "terminal", &schema()).unwrap();
        assert_eq!(o, Origin::BareString);
        assert_eq!(v["command"], "ls -la");
    }

    #[test]
    fn malformed_object_refuses_instead_of_dispatching() {
        let err = parse_arguments("{command: ls}", "terminal", &schema()).unwrap_err();
        assert!(err.contains("was NOT called"), "{err}");
        assert!(err.contains("expected arguments"), "{err}");
    }

    #[test]
    fn coercion_fixes_unambiguous_types() {
        let mut v = serde_json::json!({"command": "ls", "timeout": "30"});
        let mut notes = Vec::new();
        coerce_args(&mut v, &schema(), &mut notes);
        assert_eq!(v["timeout"], 30);
        assert_eq!(notes.len(), 1);
    }

    fn same(n: &str) -> String {
        if n == "bash" {
            "terminal".into()
        } else {
            n.to_string()
        }
    }

    fn batch_schema() -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"invocations": {"type": "array"}}
        })
    }

    /// Session 5de12a's batch, byte for byte in shape: the invocations
    /// are right, but they arrive as a JSON string inside an envelope
    /// that names `tool_call`.
    #[test]
    fn a_batch_wrapped_in_a_call_envelope_is_opened() {
        let raw = r#"{"arguments": "{\"invocations\": [{\"arguments\": {\"query\": \"a\"}, \"name\": \"web_search\"}, {\"arguments\": {\"query\": \"b\"}, \"name\": \"web_search\"}]}", "name": "tool_call"}"#;
        let (mut v, _) = parse_arguments(raw, "batch", &batch_schema()).unwrap();
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "batch", &batch_schema(), &same, &mut notes).unwrap();
        let items = v["invocations"].as_array().expect("invocations opened");
        assert_eq!(items.len(), 2);
        assert_eq!(items[1]["arguments"]["query"], "b");
        assert!(notes[0].contains("wrapped"), "{notes:?}");
    }

    #[test]
    fn a_terminal_envelope_naming_itself_or_an_alias_is_opened() {
        for name in ["terminal", "bash"] {
            let mut v = serde_json::json!({
                "arguments": "{\"command\": \"head -c 10 x\"}",
                "name": name
            });
            let mut notes = Vec::new();
            unwrap_call_envelope(&mut v, "terminal", &schema(), &same, &mut notes).unwrap();
            assert_eq!(v, serde_json::json!({"command": "head -c 10 x"}), "{name}");
        }
        // An object inside, envelopes two deep, no name at all.
        let mut v = serde_json::json!({"arguments": {"arguments": {"command": "id"}}});
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "terminal", &schema(), &same, &mut notes).unwrap();
        assert_eq!(v["command"], "id");
        assert_eq!(notes.len(), 1, "one note however deep: {notes:?}");
    }

    /// Running the inner call would run a tool the model did not call.
    /// Refused, naming the tool it did name.
    #[test]
    fn an_envelope_naming_another_tool_is_refused() {
        let mut v = serde_json::json!({"name": "write_file", "arguments": {"path": "x"}});
        let mut notes = Vec::new();
        let err = unwrap_call_envelope(&mut v, "terminal", &schema(), &same, &mut notes)
            .expect_err("a call to write_file sent to terminal must not run either");
        assert!(
            err.contains("NOT called") && err.contains("`write_file`"),
            "{err}"
        );
        assert_eq!(v["name"], "write_file", "the arguments are left as sent");
    }

    #[test]
    fn a_tool_with_its_own_arguments_or_name_field_is_left_alone() {
        let own_args = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "arguments": {"type": "object"}}
        });
        let sent = serde_json::json!({"name": "mcp_thing", "arguments": {"q": 1}});
        let mut v = sent.clone();
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "some_mcp_bridge", &own_args, &same, &mut notes).unwrap();
        assert_eq!(v, sent);
        let own_name = serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string"}}
        });
        let sent = serde_json::json!({"name": "my-skill", "arguments": {}});
        let mut v = sent.clone();
        unwrap_call_envelope(&mut v, "skill_view", &own_name, &same, &mut notes).unwrap();
        assert_eq!(v, sent);
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn plain_arguments_pass_untouched() {
        let mut v = serde_json::json!({"command": "ls", "timeout": 5});
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "terminal", &schema(), &same, &mut notes).unwrap();
        assert_eq!(v, serde_json::json!({"command": "ls", "timeout": 5}));
        assert!(notes.is_empty());
    }

    /// Session 5de12a, msg 830: `tool_call` naming `tool_call`. The gate
    /// judged the inner name and denied a guest the `mcp` capability for
    /// a web_extract.
    #[test]
    fn tool_call_inside_tool_call_is_flattened_to_the_real_call() {
        let tc = serde_json::json!({"type": "object", "properties": {"name": {}, "arguments": {}}});
        let mut v = serde_json::json!({
            "name": "tool_call",
            "arguments": {"name": "web_extract", "arguments": {"url": "https://x/"}}
        });
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "tool_call", &tc, &same, &mut notes).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"name": "web_extract", "arguments": {"url": "https://x/"}})
        );
        assert!(notes[0].contains("web_extract"), "{notes:?}");
        // As a JSON string too.
        let mut v = serde_json::json!({
            "name": "tool_call",
            "arguments": "{\"name\": \"web_search\", \"arguments\": {\"query\": \"q\"}}"
        });
        unwrap_call_envelope(&mut v, "tool_call", &tc, &same, &mut notes).unwrap();
        assert_eq!(v["name"], "web_search");
        // An ordinary tool_call is untouched.
        let sent = serde_json::json!({"name": "web_search", "arguments": {"query": "q"}});
        let mut v = sent.clone();
        let mut notes = Vec::new();
        unwrap_call_envelope(&mut v, "tool_call", &tc, &same, &mut notes).unwrap();
        assert_eq!(v, sent);
        assert!(notes.is_empty());
    }

    /// Msg 784: the innermost object names no tool at all. Refused with
    /// the shape to send, not denied a capability.
    #[test]
    fn tool_call_naming_itself_with_no_call_inside_is_refused() {
        let tc = serde_json::json!({"type": "object", "properties": {"name": {}, "arguments": {}}});
        let mut v = serde_json::json!({
            "name": "tool_call",
            "arguments": {"arguments": {"query": "q"}, "name": "tool_call"}
        });
        let mut notes = Vec::new();
        let err = unwrap_call_envelope(&mut v, "tool_call", &tc, &same, &mut notes)
            .expect_err("no tool named anywhere");
        assert!(
            err.contains("NOT called") && err.contains("web_search"),
            "{err}"
        );
    }

    #[test]
    fn edit_distance_agrees_with_common_typos() {
        assert_eq!(edit_distance("termnial", "terminal"), 2);
        assert_eq!(edit_distance("read", "read_file"), 5);
        assert_eq!(edit_distance("terminal", "terminal"), 0);
    }

    // ── parameter-name aliases ──────────────────────────────────────

    fn patch_schema() -> Value {
        serde_json::json!({"type":"object","properties":{
            "path": {"type":"string"},
            "old_string": {"type":"string"},
            "new_string": {"type":"string"}
        }})
    }

    /// THE case: a model trained on Claude Code emits `file_path`.
    /// That is valid JSON, so the transport ladder passes it straight
    /// through and the tool then fails on a missing `path` — and an
    /// RL'd model emits the same name again next round. One rename
    /// saves the loop.
    #[test]
    fn file_path_is_renamed_to_path() {
        let mut args = serde_json::json!({
            "file_path": "/tmp/x.rs", "old_string": "a", "new_string": "b"
        });
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &patch_schema(), &mut notes);
        assert_eq!(args["path"], "/tmp/x.rs");
        assert!(args.get("file_path").is_none(), "the old key is gone");
        assert_eq!(notes.len(), 1, "and the model is told: {notes:?}");
        assert!(notes[0].contains("file_path"), "{notes:?}");
    }

    /// The Codex shape: old_str/new_str mean exactly old_string/
    /// new_string, and nothing else in this tool does.
    #[test]
    fn the_str_replace_editor_names_are_renamed() {
        let mut args = serde_json::json!({"path":"/x","old_str":"a","new_str":"b"});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &patch_schema(), &mut notes);
        assert_eq!(args["old_string"], "a");
        assert_eq!(args["new_string"], "b");
        assert_eq!(notes.len(), 2);
    }

    /// A tool whose schema REALLY takes `file_path` keeps it. The
    /// rename is schema-guarded, not a blanket string substitution.
    #[test]
    fn a_tool_that_wants_file_path_keeps_it() {
        let schema = serde_json::json!({"type":"object","properties":{
            "file_path": {"type":"string"}
        }});
        let mut args = serde_json::json!({"file_path": "/tmp/x"});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &schema, &mut notes);
        assert_eq!(args["file_path"], "/tmp/x");
        assert!(notes.is_empty(), "nothing to say: {notes:?}");
    }

    /// If the real name is ALREADY set, the alias must not clobber it.
    /// Two keys meaning the same field is the model's mistake to fix,
    /// and silently picking one would hide it.
    #[test]
    fn an_existing_target_is_never_clobbered() {
        let mut args = serde_json::json!({"path":"/real","file_path":"/other"});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &patch_schema(), &mut notes);
        assert_eq!(args["path"], "/real", "the real key wins untouched");
        assert_eq!(
            args["file_path"], "/other",
            "and the stray one is left visible"
        );
        assert!(notes.is_empty());
    }

    /// A field the schema does not know stays put — repair fixes
    /// transport and naming, never intent.
    #[test]
    fn an_unknown_field_is_left_alone() {
        let mut args = serde_json::json!({"path":"/x","wibble":1});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &patch_schema(), &mut notes);
        assert_eq!(args["wibble"], 1);
        assert!(notes.is_empty());
    }
    // ── RM3: the payloads qwen-flash actually sent ──────────────────

    fn clarify_schema() -> Value {
        serde_json::json!({"type":"object","properties":{
            "question": {"type":"string"},
            "choices": {"type":"array", "items": {"type":"string"}},
            "multi_select": {"type":"boolean"}
        },"required":["question"]})
    }

    /// USER (RM3 #3): qwen-flash called clarify with `questions` — a
    /// key whose value was a JSON-ENCODED STRING of an array of
    /// {question, choices} objects — and got "error: question is
    /// required". The rename once moved it under `question`; the
    /// 2026-09-26 parity round made `questions` a NATIVE clarify
    /// parameter instead (one dialog, several questions), so the
    /// rename row is GONE and a plural payload must now arrive
    /// untouched — the tool converts it itself.
    #[test]
    fn a_native_questions_key_is_never_renamed_away() {
        let mut args = serde_json::json!({
            "questions": [{"question":"How?","choices":["a","b"]}]
        });
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &clarify_schema(), &mut notes);
        assert!(
            args.get("questions").is_some(),
            "the native key survives: {args}"
        );
        assert!(args.get("question").is_none());
    }

    /// The transport half: a JSON array handed over as a STRING for an
    /// array-typed field is unwrapped, exactly like the object case.
    /// Without this, `choices` arrives as one giant string and every
    /// option welds into one.
    #[test]
    fn stringified_array_is_unwrapped_for_array_fields() {
        let mut args = serde_json::json!({
            "question": "How?",
            "choices": "[\"No floating tip\",\"Tip everywhere\"]"
        });
        let mut notes = Vec::new();
        coerce_args(&mut args, &clarify_schema(), &mut notes);
        assert!(args["choices"].is_array(), "unwrapped: {args}");
        assert_eq!(args["choices"][0], "No floating tip");
        assert!(!notes.is_empty());
    }

    /// Cline/Roo's replace_in_file vocabulary: search/replace (and
    /// replace_text) mean exactly old_string/new_string, and Roo's
    /// target_path/target_file mean path. Schema-guarded as always.
    #[test]
    fn search_replace_and_target_path_families_rename() {
        let mut args = serde_json::json!({
            "target_path": "/x",
            "search": "a",
            "replace_text": "b"
        });
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &patch_schema(), &mut notes);
        assert_eq!(args["path"], "/x");
        assert_eq!(args["old_string"], "a");
        assert_eq!(args["new_string"], "b");
        assert!(args.get("target_path").is_none());
        assert!(args.get("search").is_none());
        assert!(args.get("replace_text").is_none());
    }

    // ── delegate_task's Claude-Code `Task` shape ────────────────────

    fn delegate_schema() -> Value {
        serde_json::json!({"type":"object","properties":{
            "goal": {"type":"string"},
            "context": {"type":"string"},
            "agent": {"type":"string"},
            "background": {"type":"boolean"}
        }})
    }

    /// Claude Code's `Task` tool is `{description, prompt, subagent_type}`.
    /// The two that mean something here — `prompt` (the task) and
    /// `subagent_type` (which defined subagent) — rename onto `goal` and
    /// `agent`. A model trained on Claude that emits the Task shape then
    /// actually delegates instead of getting "'goal' is required".
    #[test]
    fn the_task_shape_renames_onto_delegate() {
        let mut args = serde_json::json!({
            "prompt": "audit the subagent wiring",
            "subagent_type": "auditor"
        });
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &delegate_schema(), &mut notes);
        assert_eq!(args["goal"], "audit the subagent wiring");
        assert_eq!(args["agent"], "auditor");
        assert!(args.get("prompt").is_none());
        assert!(args.get("subagent_type").is_none());
        assert_eq!(notes.len(), 2, "both renames are reported: {notes:?}");
    }

    /// `description` is Task's SHORT LABEL, not the goal. It must NOT be
    /// renamed onto `goal`: a three-word label as a child's whole task is
    /// a silent mislabel, worse than the refusal that teaches. So it is
    /// left where it is (unknown field), and `prompt` still becomes the
    /// goal on its own.
    #[test]
    fn the_task_description_is_not_mistaken_for_the_goal() {
        let mut args = serde_json::json!({
            "description": "Audit subagents",
            "prompt": "Read crates/veles-core/src/delegate.rs and report the child spawn path."
        });
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &delegate_schema(), &mut notes);
        assert_eq!(
            args["description"], "Audit subagents",
            "the label is left alone"
        );
        assert!(
            args["goal"].as_str().unwrap().starts_with("Read crates"),
            "and the real task still lands on goal: {args}"
        );
        assert_eq!(notes.len(), 1, "only prompt renamed: {notes:?}");
    }

    /// Schema-guarded the other way: a2a genuinely takes `agent` and has
    /// no `subagent_type`, imagegen/cronjob genuinely take `prompt`. The
    /// `prompt`→`goal` and `subagent_type`→`agent` rows must not touch a
    /// schema that really wants those names.
    #[test]
    fn a_tool_that_really_takes_prompt_or_agent_keeps_them() {
        let cron_schema = serde_json::json!({"type":"object","properties":{
            "prompt": {"type":"string"}, "schedule": {"type":"string"}
        }});
        let mut args = serde_json::json!({"prompt": "nightly check", "schedule": "0 9 * * *"});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &cron_schema, &mut notes);
        assert_eq!(
            args["prompt"], "nightly check",
            "cronjob's prompt is its own"
        );
        assert!(notes.is_empty(), "{notes:?}");

        let a2a_schema = serde_json::json!({"type":"object","properties":{
            "action": {"type":"string"}, "agent": {"type":"string"}, "message": {"type":"string"}
        }});
        let mut args = serde_json::json!({"action": "call", "agent": "peer", "message": "hi"});
        let mut notes = Vec::new();
        rename_param_aliases(&mut args, &a2a_schema, &mut notes);
        assert_eq!(args["agent"], "peer", "a2a's agent is its own");
        assert!(notes.is_empty(), "{notes:?}");
    }

    // ── RM3: the implicit current file ──────────────────────────────

    /// USER (RM3 #5): models expect consecutive file ops to reuse the
    /// previous path when none is given ("patch {old_string, new_string}"
    /// after a read_file). The refusal burned a round and taught
    /// nothing. The filler: focus present + path absent + the tool takes
    /// a path → inject, and SAY SO.
    #[test]
    fn implicit_path_fills_from_focus_and_says_so() {
        let mut args = serde_json::json!({"old_string":"a","new_string":"b"});
        let mut notes = Vec::new();
        fill_implicit_path(
            "patch",
            Some("/srv/app/src/main.rs"),
            &patch_schema(),
            &mut args,
            &mut notes,
        );
        assert_eq!(args["path"], "/srv/app/src/main.rs");
        assert!(notes.iter().any(|n| n.contains("main.rs")), "{notes:?}");
    }

    /// An explicit path is never overridden — inference is a default,
    /// not a correction.
    #[test]
    fn an_explicit_path_beats_the_focus() {
        let mut args = serde_json::json!({"path":"/explicit.rs","old_string":"a","new_string":"b"});
        let mut notes = Vec::new();
        fill_implicit_path(
            "patch",
            Some("/focused.rs"),
            &patch_schema(),
            &mut args,
            &mut notes,
        );
        assert_eq!(args["path"], "/explicit.rs");
        assert!(notes.is_empty());
    }

    /// No focus recorded → the old refusal path, unchanged.
    #[test]
    fn no_focus_no_injection() {
        let mut args = serde_json::json!({"old_string":"a","new_string":"b"});
        let mut notes = Vec::new();
        fill_implicit_path("patch", None, &patch_schema(), &mut args, &mut notes);
        assert!(args.get("path").is_none());
        assert!(notes.is_empty());
    }

    /// A tool that does not take `path` at all is not a file op.
    #[test]
    fn non_file_tools_are_untouched() {
        let mut args = serde_json::json!({"query":"x"});
        let mut notes = Vec::new();
        fill_implicit_path(
            "terminal",
            Some("/focused.rs"),
            &schema(),
            &mut args,
            &mut notes,
        );
        assert!(args.get("path").is_none());
        assert!(notes.is_empty());
    }
    /// Claude Code's background-task tools land on `shell` with their
    /// arguments reshaped: ids from bash_id/task_id/shell_id (strings
    /// too), TaskOutput blocking by default, milliseconds → seconds,
    /// and the Kill/Stop family as `kill`. No id is a teaching refusal.
    #[test]
    fn claude_codes_background_task_tools_become_shell_calls() {
        let run = |name: &str, raw: &str| match convert_alias_shape(name, raw) {
            ShapeOutcome::Run { target, args, .. } => (target, args),
            other => panic!("{name} {raw}: expected a conversion, got {other:?}"),
        };
        let (t, a) = run("TaskOutput", r#"{"task_id": "34", "timeout": 120000}"#);
        assert_eq!(t, "shell");
        assert_eq!(
            a,
            serde_json::json!({"action": "wait", "id": 34, "timeout": 120})
        );
        let (_, a) = run("TaskOutput", r#"{"task_id": 34, "block": false}"#);
        assert_eq!(a["action"], "output");
        let (_, a) = run("BashOutput", r#"{"bash_id": "7"}"#);
        assert_eq!(a, serde_json::json!({"action": "output", "id": 7}));
        for name in ["KillShell", "KillBash", "TaskStop"] {
            let (t, a) = run(name, r#"{"shell_id": 9}"#);
            assert_eq!(t, "shell");
            assert_eq!(a, serde_json::json!({"action": "kill", "id": 9}), "{name}");
        }
        assert!(matches!(
            convert_alias_shape("TaskStop", r#"{"task_id": "bash_abc"}"#),
            ShapeOutcome::Refuse(_)
        ));
    }
}
