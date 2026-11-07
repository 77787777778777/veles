//! Session export: ONE renderer and ONE private writer for every surface.
//!
//! #33 in the feature matrix. The model's `session_export` tool, the
//! shared `/export` command (TUI, Telegram, IRC, a desktop slash line)
//! and the desktop's own export (`GET /api/sessions/:id/export`, which
//! the Tk side writes to a local file) all render through this module.
//! There used to be three renderers: the tool's here, a copy in
//! `commands.rs` that dropped tool calls and wrote a predictable
//! world-readable `/tmp/veles_export_<sid>.md`, and a Tcl one that read
//! the desktop history shape, failed on every engine-note row (no
//! `content` key) and flattened nested JSON into Tcl list strings.
//!
//! Files land in [`exports_dir`] (`<data_dir>/exports`), deliberately
//! OUTSIDE the media root: a below-owner principal's `MEDIA:` marker
//! may name anything under `media/` (`media::deliverable`), and an
//! owner transcript must not be one marker away from a guest.

use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::lockutil::LockOk;
use crate::providers::ChatMessage;
use crate::store::{Store, StoredNote};
use crate::tools::Tool;

/// The two formats every surface offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Json,
}

impl Format {
    /// `markdown`/`md` (or nothing) and `json`; anything else is refused
    /// rather than silently exported as Markdown.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "markdown" | "md" => Some(Format::Markdown),
            "json" => Some(Format::Json),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Json => "json",
        }
    }
    pub fn ext(&self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Json => "json",
        }
    }
}

/// A rendered transcript and how many messages it holds.
pub struct Rendered {
    pub content: String,
    pub message_count: usize,
}

/// Render a whole session (messages, engine notes, title) from the store.
/// `tool_limit` bounds each tool result's text: the model-facing tool
/// passes one so an export does not flood its own context; a file export
/// passes `None` and keeps every byte.
pub fn render_session(
    store: &Store,
    sid: &str,
    format: Format,
    tool_limit: Option<usize>,
) -> anyhow::Result<Rendered> {
    let messages = store.load_history(sid)?;
    let notes = store.notes_for(sid).unwrap_or_default();
    let title = store
        .title(sid)
        .ok()
        .flatten()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| sid.to_string());
    let content = match format {
        Format::Markdown => export_markdown(sid, &title, &messages, &notes, tool_limit),
        Format::Json => export_json(sid, &title, &messages, &notes),
    };
    Ok(Rendered {
        content,
        message_count: messages.len(),
    })
}

/// Where exported transcripts are written: `<data_dir>/exports`.
pub fn exports_dir() -> PathBuf {
    crate::config::data_dir().join("exports")
}

/// A session id as a file-name component on every platform: anything
/// outside `[A-Za-z0-9_.-]` becomes `_`. A `:` on NTFS names an
/// alternate data stream, and a separator would choose a directory.
pub fn file_stem(sid: &str) -> String {
    let stem: String = sid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let stem = stem.trim_matches('.');
    if stem.is_empty() {
        "session".to_string()
    } else {
        stem.to_string()
    }
}

/// Write `content` as a NEW file only this user can read, in `dir`.
/// `create_new` refuses to follow a planted symlink or reuse a file;
/// the name carries a timestamp, and a collision gets a counter.
pub fn write_private_new(
    dir: &Path,
    sid: &str,
    format: Format,
    content: &str,
) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    let stem = file_stem(sid);
    for n in 0..100u32 {
        let name = if n == 0 {
            format!("veles-{stem}-{stamp}.{}", format.ext())
        } else {
            format!("veles-{stem}-{stamp}-{n}.{}", format.ext())
        };
        let path = dir.join(name);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            o.mode(0o600);
        }
        match o.open(&path) {
            Ok(mut f) => {
                f.write_all(content.as_bytes())?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free export file name",
    ))
}

/// Render and write a session export into [`exports_dir`].
pub fn export_to_file(
    store: &Store,
    sid: &str,
    format: Format,
) -> anyhow::Result<(PathBuf, usize)> {
    let rendered = render_session(store, sid, format, None)?;
    let path = write_private_new(&exports_dir(), sid, format, &rendered.content)?;
    Ok((path, rendered.message_count))
}

pub struct SessionExportTool {
    pub store: Arc<std::sync::Mutex<crate::store::Store>>,
}

#[async_trait]
impl Tool for SessionExportTool {
    fn name(&self) -> &str {
        "session_export"
    }
    fn description(&self) -> &str {
        "Export a chat session as markdown. Reads all messages and formats them with headers, tool calls, and reasoning. Useful for handoffs, documentation, or archiving."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Session ID to export. If omitted, exports the most recent session."
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "json"],
                    "description": "Output format (default markdown)"
                }
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(format) = Format::parse(args["format"].as_str().unwrap_or("markdown")) else {
            return "error: format must be markdown or json".into();
        };
        let session_id = args["session_id"].as_str().map(|s| s.to_string());

        let store = self.store.lock_ok();

        // Use the provided session, or the most recent one
        let sid = match session_id {
            Some(s) if !s.is_empty() => s,
            _ => match store.latest_session() {
                Ok(Some(s)) => s,
                _ => return "no sessions found".into(),
            },
        };

        match render_session(&store, &sid, format, Some(5000)) {
            Ok(r) => r.content,
            Err(e) => format!("error reading session: {e}"),
        }
    }
}

/// The reasoning a surface shows: the display copy (a plugin's summary)
/// when there is one, else the raw chain the provider returned.
fn shown_reasoning(m: &ChatMessage) -> Option<&str> {
    m.display_reasoning
        .as_deref()
        .or(m.reasoning.as_deref())
        .filter(|r| !r.trim().is_empty())
}

/// Cut `s` to at most `limit` bytes on a character boundary.
fn clip(s: &str, limit: Option<usize>) -> (&str, bool) {
    match limit {
        Some(limit) if s.len() > limit => {
            let mut end = limit;
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            (&s[..end], true)
        }
        _ => (s, false),
    }
}

/// A fence that the body cannot close: one backtick longer than the
/// longest run inside it.
fn fence_for(body: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

fn push_note(out: &mut String, n: &StoredNote) {
    out.push_str(&format!("> *Note ({}):* {}\n", n.kind, n.summary));
    if !n.detail.trim().is_empty() {
        for line in n.detail.lines() {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
}

fn push_tool_calls(out: &mut String, calls: &Value) {
    let Some(calls) = calls.as_array() else {
        return;
    };
    for call in calls {
        let name = call["function"]["name"].as_str().unwrap_or("?");
        let raw = &call["function"]["arguments"];
        // Arguments arrive as a JSON string on the wire; pretty-print it
        // when it parses, and keep it verbatim when it does not.
        let args = match raw {
            Value::String(s) => serde_json::from_str::<Value>(s)
                .ok()
                .and_then(|v| serde_json::to_string_pretty(&v).ok())
                .unwrap_or_else(|| s.clone()),
            Value::Null => String::new(),
            other => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        let fence = fence_for(&args);
        out.push_str(&format!(
            "<details><summary>Tool call: {name}</summary>\n\n{fence}json\n{args}\n{fence}\n\n</details>\n\n"
        ));
    }
}

pub fn export_markdown(
    sid: &str,
    title: &str,
    messages: &[ChatMessage],
    notes: &[StoredNote],
    tool_limit: Option<usize>,
) -> String {
    let mut out = String::with_capacity(8192);
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!(
        "> Session `{sid}` · {} messages\n\n",
        messages.len()
    ));
    out.push_str("---\n\n");

    // Notes are pinned after the message they followed (0 = before any);
    // walk them in alongside the messages, the way the transcript does.
    let mut ni = 0usize;
    while ni < notes.len() && notes[ni].after_msg_id <= 0 {
        push_note(&mut out, &notes[ni]);
        ni += 1;
    }

    for msg in messages {
        match msg.role.as_str() {
            "user" => match &msg.injection {
                // An engine injection is never the human's words.
                Some(class) => {
                    out.push_str(&format!("> **[{class}]** "));
                    out.push_str(&msg.content.replace('\n', "\n> "));
                    out.push_str("\n\n");
                }
                None => {
                    out.push_str("## User\n\n");
                    out.push_str(&msg.content);
                    out.push_str("\n\n");
                }
            },
            "assistant" => {
                out.push_str("## Assistant\n\n");
                if let Some(r) = shown_reasoning(msg) {
                    out.push_str("<details><summary>Reasoning</summary>\n\n");
                    out.push_str(r);
                    out.push_str("\n\n</details>\n\n");
                }
                if !msg.content.is_empty() {
                    out.push_str(&msg.content);
                    out.push_str("\n\n");
                }
                if let Some(calls) = &msg.tool_calls {
                    push_tool_calls(&mut out, calls);
                }
            }
            "tool" => {
                out.push_str("<details><summary>Tool result");
                if let Some(meta) = &msg.tool_meta {
                    out.push_str(&format!(": {}", meta.name));
                }
                let (body, cut) = clip(&msg.content, tool_limit);
                let fence = fence_for(body);
                out.push_str(&format!("</summary>\n\n{fence}\n"));
                out.push_str(body);
                if cut {
                    out.push_str("\n[...truncated]");
                }
                out.push_str(&format!("\n{fence}\n\n</details>\n\n"));
            }
            "system" => {
                out.push_str("> **System:** ");
                out.push_str(&msg.content);
                out.push_str("\n\n");
            }
            _ => {
                out.push_str(&format!("### {}\n\n", msg.role));
                out.push_str(&msg.content);
                out.push_str("\n\n");
            }
        }
        if let Some(id) = msg.msg_id {
            while ni < notes.len() && notes[ni].after_msg_id <= id {
                push_note(&mut out, &notes[ni]);
                ni += 1;
            }
        }
    }
    // Anything pinned past the last message.
    while ni < notes.len() {
        push_note(&mut out, &notes[ni]);
        ni += 1;
    }

    out.push_str("---\n\n");
    out.push_str(&format!(
        "*Exported by veles-agent · {}*\n",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    ));
    out
}

/// The full-fidelity JSON: every field a message row carries, typed
/// (tool calls stay JSON, not strings), plus the engine notes.
pub fn export_json(
    sid: &str,
    title: &str,
    messages: &[ChatMessage],
    notes: &[StoredNote],
) -> String {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| {
            let mut v = serde_json::json!({
                "role": m.role,
                "content": m.content,
                "reasoning": m.reasoning,
            });
            if let Some(d) = &m.display_reasoning {
                v["display_reasoning"] = Value::String(d.clone());
            }
            if let Some(tc) = &m.tool_calls {
                v["tool_calls"] = tc.clone();
            }
            if let Some(meta) = &m.tool_meta {
                v["tool_call_id"] = Value::String(meta.tool_call_id.clone());
                v["tool_name"] = Value::String(meta.name.clone());
            }
            if let Some(inj) = &m.injection {
                v["injection"] = Value::String(inj.clone());
            }
            if let Some(id) = m.msg_id {
                v["msg_id"] = Value::from(id);
            }
            v
        })
        .collect();
    let notes: Vec<Value> = notes
        .iter()
        .map(|n| {
            serde_json::json!({
                "after_msg_id": n.after_msg_id,
                "kind": n.kind,
                "summary": n.summary,
                "detail": n.detail,
                "at": n.created_at,
            })
        })
        .collect();

    serde_json::to_string_pretty(&serde_json::json!({
        "session_id": sid,
        "title": title,
        "message_count": messages.len(),
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "messages": msgs,
        "notes": notes,
    }))
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session shaped like a real one: a prompt, an origin injection,
    /// a tool round with nested arguments, a multi-byte tool result,
    /// reasoning in both copies, and engine notes before, between and
    /// after the messages.
    fn real_shaped(store: &Store) -> &'static str {
        let sid = "20261106_010203_abc123";
        store.create_session(sid).unwrap();
        store
            .record_note(sid, "cron", "a cron job opened this session", None, None)
            .unwrap();
        store
            .append_message(
                sid,
                &ChatMessage::user("[origin] desktop").with_injection("origin:desktop"),
            )
            .unwrap();
        store
            .append_message(sid, &ChatMessage::user("list the files"))
            .unwrap();
        let mut call = ChatMessage::assistant("", Some("the saved reasoning".into()));
        call.tool_calls = Some(serde_json::json!([{
            "id": "c1", "type": "function",
            "function": {"name": "terminal", "arguments": "{\"cmd\":\"ls\"}"}
        }]));
        store.append_message(sid, &call).unwrap();
        store
            .record_note(
                sid,
                "compaction",
                "history was compacted",
                Some("12 rows"),
                None,
            )
            .unwrap();
        store
            .append_message(
                sid,
                &ChatMessage::tool_result("c1", "terminal", "файл ```".repeat(1000)),
            )
            .unwrap();
        store
            .append_message(sid, &ChatMessage::assistant("two files", None))
            .unwrap();
        store
            .record_note(sid, "restart", "the gateway restarted", None, None)
            .unwrap();
        sid
    }

    #[test]
    fn markdown_renders_every_row_kind_of_a_real_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let sid = real_shaped(&store);
        let md = render_session(&store, sid, Format::Markdown, None)
            .unwrap()
            .content;
        assert!(md.contains("## User\n\nlist the files"), "{md}");
        assert!(
            md.contains("> **[origin:desktop]**"),
            "injection labelled, not speech"
        );
        assert!(!md.contains("## User\n\n[origin]"));
        assert!(
            md.contains("<summary>Reasoning</summary>\n\nthe saved reasoning"),
            "reasoning is exported"
        );
        assert!(md.contains("Tool call: terminal"));
        assert!(
            md.contains("\"cmd\": \"ls\""),
            "arguments are pretty JSON: {md}"
        );
        assert!(md.contains("Tool result: terminal"));
        assert!(md.contains("two files"));
        let cron = md.find("a cron job opened").unwrap();
        let compact = md.find("history was compacted").unwrap();
        let restart = md.find("the gateway restarted").unwrap();
        assert!(cron < md.find("list the files").unwrap());
        assert!(compact > md.find("Tool call: terminal").unwrap());
        assert!(compact < md.find("Tool result: terminal").unwrap());
        assert!(restart > md.find("two files").unwrap());
        assert!(md.contains("> 12 rows"));
        // The tool body holds ``` runs; its fence must be longer.
        assert!(md.contains("````"), "a fence the body cannot close");
    }

    #[test]
    fn the_tool_path_clips_on_a_character_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let sid = real_shaped(&store);
        // 5000 falls inside a two-byte Cyrillic letter for this body; a
        // byte slice there panicked.
        let md = render_session(&store, sid, Format::Markdown, Some(5000))
            .unwrap()
            .content;
        assert!(md.contains("[...truncated]"));
        for limit in 4990..5010 {
            let _ = render_session(&store, sid, Format::Markdown, Some(limit)).unwrap();
        }
    }

    #[test]
    fn json_keeps_nested_fields_typed_and_includes_notes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let sid = real_shaped(&store);
        let json = render_session(&store, sid, Format::Json, None)
            .unwrap()
            .content;
        let v: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["session_id"], sid);
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 5);
        let call = msgs.iter().find(|m| m["tool_calls"].is_array()).unwrap();
        assert_eq!(call["tool_calls"][0]["function"]["name"], "terminal");
        assert_eq!(call["reasoning"], "the saved reasoning");
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_name"], "terminal");
        assert_eq!(tool["tool_call_id"], "c1");
        assert!(tool["content"].as_str().unwrap().starts_with("файл"));
        assert!(msgs[0]["msg_id"].is_i64());
        assert_eq!(msgs[0]["injection"], "origin:desktop");
        assert_eq!(v["notes"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn file_stems_are_safe_on_every_platform() {
        assert_eq!(file_stem("20261106_010203_abc"), "20261106_010203_abc");
        assert_eq!(file_stem("irc:net:#chan"), "irc_net__chan");
        assert_eq!(file_stem("../../etc"), "_.._etc");
        assert_eq!(file_stem(".."), "session");
        assert!(!file_stem("a\\b/c:d").contains(['\\', '/', ':']));
    }

    #[test]
    fn exports_are_new_private_files_outside_the_media_root() {
        let (_home, _g) = crate::testenv::scratch_home();
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let sid = real_shaped(&store);
        let (path, count) = export_to_file(&store, sid, Format::Markdown).unwrap();
        assert_eq!(count, 5);
        assert!(path.starts_with(exports_dir()));
        assert!(!path.starts_with(crate::config::media_dir()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Two exports in the same millisecond never share (or follow) a file.
        let (again, _) = export_to_file(&store, sid, Format::Markdown).unwrap();
        assert_ne!(path, again);
        // A below-owner principal's MEDIA: marker cannot name it.
        let guest = crate::acl::Principal::known("irc:guest", crate::acl::Role::User);
        assert!(crate::media::deliverable(&path.display().to_string(), Some(&guest)).is_some());
    }

    #[test]
    fn unknown_formats_are_refused() {
        assert_eq!(Format::parse("md"), Some(Format::Markdown));
        assert_eq!(Format::parse("JSON"), Some(Format::Json));
        assert_eq!(Format::parse(""), Some(Format::Markdown));
        assert_eq!(Format::parse("xml"), None);
    }
}
