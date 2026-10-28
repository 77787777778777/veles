//! Session export tool — export a session as markdown.
//!
//! #33 in the feature matrix. Reads all messages from a session and
//! formats them as a clean markdown document. The model can call this
//! to produce handoff docs, share conversation logs, or archive work.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::lockutil::LockOk;
use crate::providers::ChatMessage;
use crate::tools::Tool;

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
        let format = args["format"].as_str().unwrap_or("markdown");
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

        let messages = match store.load_history(&sid) {
            Ok(m) => m,
            Err(e) => return format!("error reading session: {e}"),
        };

        let title = store
            .title(&sid)
            .ok()
            .flatten()
            .unwrap_or_else(|| sid.clone());

        if format == "json" {
            export_json(&sid, &title, &messages)
        } else {
            export_markdown(&sid, &title, &messages)
        }
    }
}

fn export_markdown(sid: &str, title: &str, messages: &[ChatMessage]) -> String {
    let mut out = String::with_capacity(8192);
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!(
        "> Session `{sid}` · {} messages\n\n",
        messages.len()
    ));
    out.push_str("---\n\n");

    for msg in messages {
        match msg.role.as_str() {
            "user" => {
                out.push_str("## User\n\n");
                out.push_str(&msg.content);
                out.push_str("\n\n");
            }
            "assistant" => {
                out.push_str("## Assistant\n\n");
                if let Some(r) = &msg.reasoning {
                    if !r.is_empty() {
                        out.push_str("<details><summary>Reasoning</summary>\n\n");
                        out.push_str(r);
                        out.push_str("\n\n</details>\n\n");
                    }
                }
                out.push_str(&msg.content);
                out.push_str("\n\n");
            }
            "tool" => {
                out.push_str("<details><summary>Tool result");
                if let Some(meta) = &msg.tool_meta {
                    out.push_str(&format!(": {}", meta.name));
                }
                out.push_str("</summary>\n\n```\n");
                let content = &msg.content;
                if content.len() > 5000 {
                    out.push_str(&content[..5000]);
                    out.push_str("\n[...truncated]");
                } else {
                    out.push_str(content);
                }
                out.push_str("\n```\n\n</details>\n\n");
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
    }

    out.push_str("---\n\n");
    out.push_str(&format!(
        "*Exported by veles-agent · {}*",
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC")
    ));
    out
}

fn export_json(sid: &str, title: &str, messages: &[ChatMessage]) -> String {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": m.role,
                "content": m.content,
                "reasoning": m.reasoning,
            })
        })
        .collect();

    serde_json::json!({
        "session_id": sid,
        "title": title,
        "message_count": messages.len(),
        "messages": msgs,
    })
    .to_string()
}
