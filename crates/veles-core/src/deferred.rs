//! Deferred tool discovery — tool_search / tool_describe / tool_call.
//!
//! Hermes loads ~26 tool schemas into every session's system prompt;
//! the rest are "on-demand" — discoverable via `tool_search`, described
//! by `tool_describe`, invoked via `tool_call`. Veles uses the same
//! pattern for MCP-registered tools and any future deferred tools that
//! aren't worth their schema weight in every turn.
//!
//! The registry has two tiers:
//!   - **Registered** (in the ToolRegistry): always available, always
//!     in the system prompt. terminal, read_file, memory, skills, etc.
//!   - **Deferred** (in the DeferredRegistry): not in the prompt, but
//!     discoverable and invocable through these three tools.
//!
//! MCP tools that land at gateway startup are deferred — they're
//! available in every session but their full schemas are loaded on
//! demand (not burned into the system prompt every turn).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::lockutil::LockOk;
use crate::tools::Tool;

/// A deferred tool: full schema available but not in the system prompt.
pub struct DeferredTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub handler: Arc<dyn Tool>,
}

/// The GLOBAL deferred registry (MCP tools at gateway startup).
/// OWNER-TRACKED (M4/R2): each entry records the registering thread's
/// owner, so a module's deferred tools can be purged on reload without
/// touching core's. Core/MCP registrations happen on untagged threads →
/// owner `None`, never purged.
static GLOBAL: std::sync::OnceLock<crate::owned::OwnedRegistry<String, DeferredTool>> =
    std::sync::OnceLock::new();

fn global() -> &'static crate::owned::OwnedRegistry<String, DeferredTool> {
    GLOBAL.get_or_init(crate::owned::OwnedRegistry::default)
}

/// Register a deferred tool (available via tool_search/tool_call, but
/// not in the system prompt). MCP tools use this at gateway startup.
/// The owner is read from the calling thread (M4).
pub fn register_deferred(tool: DeferredTool) {
    global().insert(tool.name.clone(), tool);
}

/// Remove every deferred tool `owner` registered (R3 teardown on a module
/// reload). `None` (core) removes nothing. Returns the count removed.
pub fn purge_owner_deferred(owner: Option<&'static str>) -> usize {
    global().purge(owner)
}

pub fn deferred_names() -> Vec<String> {
    let mut v = global().keys();
    v.sort();
    v
}

pub fn deferred_describe(name: &str) -> Option<String> {
    global().with(&name.to_string(), describe_one)
}

/// The shared rendering of one deferred tool for tool_describe.
pub fn describe_one(t: &DeferredTool) -> String {
    format!(
        "{} — {}\n\nschema: {}",
        t.name,
        t.description,
        serde_json::to_string_pretty(&t.schema).unwrap_or_default()
    )
}

/// The schema of a globally (MCP) deferred tool — the registry's
/// schema_for falls through here for names it does not hold resident.
pub fn global_schema(name: &str) -> Option<Value> {
    global().with(&name.to_string(), |t| t.schema.clone())
}

/// The handler of a globally (MCP) deferred tool — dispatch falls
/// through here so a deferred tool stays invocable by its plain name.
pub fn global_handler(name: &str) -> Option<Arc<dyn Tool>> {
    global().with(&name.to_string(), |t| t.handler.clone())
}

pub async fn deferred_call(name: &str, args: Value) -> Option<String> {
    let handler = global_handler(name);
    match handler {
        Some(h) => Some(h.call(args).await),
        None => None,
    }
}

/// The session's deferred builtins plus the global (MCP) registry —
/// one iterator over everything discoverable, deduplicated by name.
fn all_deferred(session: &std::sync::Mutex<HashMap<String, DeferredTool>>) -> Vec<DeferredTool> {
    let mut out: Vec<DeferredTool> = session.lock_ok().values().map(clone_tool).collect();
    global().for_each(|name, t| {
        if !out.iter().any(|o| &o.name == name) {
            out.push(clone_tool(&t.value));
        }
    });
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// DeferredTool holds an Arc handler, so this is a cheap share, not a
/// deep copy.
fn clone_tool(t: &DeferredTool) -> DeferredTool {
    DeferredTool {
        name: t.name.clone(),
        description: t.description.clone(),
        schema: t.schema.clone(),
        handler: t.handler.clone(),
    }
}

// ── tool_search ───────────────────────────────────────────────────────

/// Discovers tools that are NOT resident in the system prompt —
/// session-tiered builtins AND MCP tools. Holds the session's deferred
/// map (shared with its registry); the global registry is consulted as
/// well.
pub struct ToolSearchTool {
    pub session: Arc<std::sync::Mutex<HashMap<String, DeferredTool>>>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Search for available tools that are NOT in the system prompt \
         (deferred tools, MCP tools). Returns name + short description \
         for each match. Use tool_describe for the full schema, then \
         tool_call to invoke."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Keywords describing the capability needed (e.g. 'web search', 'image analysis', 'browser')"
                }
            },
            "required": ["query"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let q = args["query"].as_str().unwrap_or_default().to_lowercase();
        let all = all_deferred(&self.session);
        if all.is_empty() {
            return "No deferred tools registered.".to_string();
        }
        // Word by word, with `_` and `-` read as spaces. The whole query
        // as one substring missed "web search" against `web_search` —
        // this schema's own example — and the model told IRC it had no
        // web tool (2026-10-02). A tool is listed when any word of three
        // letters or more hits; words hit in the NAME rank first, then
        // words hit anywhere.
        let norm = |s: &str| s.to_lowercase().replace(['_', '-'], " ");
        let words: Vec<String> = norm(&q)
            .split_whitespace()
            .filter(|w| w.chars().count() >= 3)
            .map(str::to_string)
            .collect();
        let mut scored = Vec::new();
        for t in &all {
            let name = norm(&t.name);
            let hay = format!("{name} {}", norm(&t.description));
            let in_name = words.iter().filter(|w| name.contains(w.as_str())).count();
            let anywhere = words.iter().filter(|w| hay.contains(w.as_str())).count();
            if q.trim().is_empty() || anywhere > 0 {
                scored.push(((in_name, anywhere), t));
            }
        }
        // `all` is sorted by name, and the sort is stable.
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        let hits: Vec<String> = scored
            .iter()
            .map(|(_, t)| {
                format!(
                    "  {} — {}",
                    t.name,
                    t.description.lines().next().unwrap_or("")
                )
            })
            .collect();
        if hits.is_empty() {
            let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
            format!(
                "No deferred tools matched '{q}'. Available: {}",
                names.join(", ")
            )
        } else {
            format!(
                "Deferred tools ({}):\n{}\nUse tool_describe for the schema, tool_call to invoke.",
                hits.len(),
                hits.join("\n")
            )
        }
    }
}

// ── tool_describe ─────────────────────────────────────────────────────

pub struct ToolDescribeTool {
    pub session: Arc<std::sync::Mutex<HashMap<String, DeferredTool>>>,
}

#[async_trait]
impl Tool for ToolDescribeTool {
    fn name(&self) -> &str {
        "tool_describe"
    }
    fn description(&self) -> &str {
        "Load the full JSON schema for a deferred tool discovered via \
         tool_search. Returns the name, description, and parameters \
         schema."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Tool name (from tool_search)"}
            },
            "required": ["name"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let name = args["name"].as_str().unwrap_or_default();
        if let Some(t) = self.session.lock_ok().get(name) {
            return describe_one(t);
        }
        match deferred_describe(name) {
            Some(desc) => desc,
            None => format!(
                "Unknown deferred tool '{name}'. Use tool_search to discover available tools."
            ),
        }
    }
}

// ── tool_call ─────────────────────────────────────────────────────────

pub struct ToolCallTool {
    pub session: Arc<std::sync::Mutex<HashMap<String, DeferredTool>>>,
}

#[async_trait]
impl Tool for ToolCallTool {
    fn name(&self) -> &str {
        "tool_call"
    }
    fn description(&self) -> &str {
        "Invoke a deferred tool (discovered via tool_search, schema \
         loaded via tool_describe). Pass arguments as a JSON object."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Tool name"},
                "arguments": {"type": "object", "description": "Arguments as a JSON object"}
            },
            "required": ["name", "arguments"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let name = args["name"].as_str().unwrap_or_default();
        let arguments = args["arguments"].clone();
        let handler = { self.session.lock_ok().get(name).map(|t| t.handler.clone()) };
        if let Some(h) = handler {
            return h.call(arguments).await;
        }
        match deferred_call(name, arguments).await {
            Some(result) => result,
            None => format!(
                "Unknown deferred tool '{name}'. Use tool_search to discover available tools."
            ),
        }
    }
}
