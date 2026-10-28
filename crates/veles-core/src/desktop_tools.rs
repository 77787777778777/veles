//! Desktop pane tools — `open_preview` and `focus_pane` (hermes parity).
//!
//! The agent drives the veles-desktop right-pane shell through these:
//! open_preview points the Preview pane at a URL or file, focus_pane
//! raises one of the panes. Both talk to the desktop API over loopback
//! (same port the Tk client uses); when the desktop isn't running they
//! say so honestly instead of erroring obscurely.
//!
//! Registered in base_registry → available in every session/frontend,
//! same as hermes' pane tools.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::tools::Tool;

/// A process-local target the pane tools consult FIRST. When set, the
/// surface that owns the window does the work in-process instead of the
/// loopback POST — the TUI installs one so `open_preview`/`focus_pane`
/// can move the TUI's own panes and open real URLs, rather than failing
/// with "desktop not running". Other processes (gateway, cron, A2A)
/// never install one and keep the loopback path.
///
/// `Some(out)` = handled, and `out` is the tool's result; `None` = not
/// handled, fall through to the desktop loopback.
type LocalTarget = Box<dyn Fn(&str, &Value) -> Option<String> + Send + Sync>;

/// OWNER-TRACKED (M4/R2): the slot records its installer so a module's
/// pane target can be purged on reload without touching a core (TUI)
/// install.
static LOCAL_TARGET: std::sync::OnceLock<crate::owned::OwnedSlot<LocalTarget>> =
    std::sync::OnceLock::new();

fn local_target_cell() -> &'static crate::owned::OwnedSlot<LocalTarget> {
    LOCAL_TARGET.get_or_init(crate::owned::OwnedSlot::default)
}

/// Install the in-process pane target (the TUI does this once at boot).
/// The owner is read from the calling thread (M4).
pub fn set_local_pane_target(f: impl Fn(&str, &Value) -> Option<String> + Send + Sync + 'static) {
    local_target_cell().set(Box::new(f));
}

/// Remove the in-process pane target (tests restore a clean process).
pub fn clear_local_pane_target() {
    local_target_cell().clear();
}

/// Uninstall the pane target ONLY if `owner` installed it (R3 teardown).
/// `None` (core) never matches. Returns whether it cleared.
pub fn purge_owner_pane_target(owner: Option<&'static str>) -> bool {
    local_target_cell().purge(owner)
}

fn local_target(name: &str, args: &Value) -> Option<String> {
    local_target_cell().with(|f| f(name, args)).flatten()
}

/// The port the gateway serves the desktop API on, by the gateway's own
/// rule (env, then `[platforms.desktop.extra] port`, then 8642). This
/// read only the env until 2026-10-11, so a configured port left every
/// loopback tool knocking on 8642.
///
/// A unit test with no port of its own gets a closed one, never 8642:
/// that is the person's running gateway, and the `config` tool's write
/// would land in their real config.toml.
fn desktop_port() -> u16 {
    if cfg!(test) && std::env::var_os("VELES_DESKTOP_PORT").is_none() {
        return 9;
    }
    crate::config::desktop_port(&crate::config::live())
}

async fn post_json(path: &str, body: Value) -> Result<Value, String> {
    desktop_post(path, body, std::time::Duration::from_secs(5)).await
}

/// The desktop API's bearer token, when it has one (`authn`).
///
/// These tools sent NONE until 2026-10-11, and the API has minted a token
/// by default since 4a1f181: from every session the gateway runs,
/// `open_preview`, `focus_pane` and `tcl_eval` were refused 401 —
/// "preview failed: {\"error\":\"unauthorized\"}", 15 times in the live
/// transcripts between 2026-08-25 and 2026-10-03.
fn with_token(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    let token = crate::authn::desktop_bearer_token();
    if token.trim().is_empty() {
        req
    } else {
        req.bearer_auth(token.trim())
    }
}

/// POST JSON to the desktop API on this host, as the owner (its token).
pub async fn desktop_post(
    path: &str,
    body: Value,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{}{}", desktop_port(), path);
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let resp = with_token(client.post(&url).json(&body))
        .send()
        .await
        .map_err(|_| {
            "the veles gateway is not running here (start it, or `veles gui`)".to_string()
        })?;
    resp.json::<Value>()
        .await
        .map_err(|e| format!("bad response from the gateway: {e}"))
}

/// GET JSON from the desktop API on this host, as the owner.
pub async fn desktop_get(path: &str) -> Result<Value, String> {
    let url = format!("http://127.0.0.1:{}{}", desktop_port(), path);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| format!("client build failed: {e}"))?;
    let resp = with_token(client.get(&url)).send().await.map_err(|_| {
        "the veles gateway is not running here (start it, or `veles gui`)".to_string()
    })?;
    resp.json::<Value>()
        .await
        .map_err(|e| format!("bad response from the gateway: {e}"))
}

pub struct OpenPreviewTool;

#[async_trait]
impl Tool for OpenPreviewTool {
    fn name(&self) -> &str {
        "open_preview"
    }
    fn description(&self) -> &str {
        "Open something in the veles-desktop Preview pane beside the chat: a web URL, a localhost dev-server URL, or a file path (text renders in the pane). Optional label for the tab."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "Web URL, localhost URL, or file path"},
                "label": {"type": "string", "description": "Optional tab label"}
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value) -> String {
        if let Some(out) = local_target("open_preview", &args) {
            return out;
        }
        let target = args["url"].as_str().unwrap_or_default().trim().to_string();
        if target.is_empty() {
            return "error: url is required".into();
        }
        let label = args["label"].as_str().map(|s| s.to_string());
        // File path vs URL: anything with a scheme is a URL; a path that
        // exists on disk is a file preview; otherwise treat as URL.
        let is_url = target.contains("://") || target.starts_with("localhost");
        let expanded = match target.strip_prefix("~/") {
            Some(rest) => match directories::UserDirs::new() {
                Some(d) => d.home_dir().join(rest).to_string_lossy().to_string(),
                None => target.clone(),
            },
            None => target.clone(),
        };
        let is_file = !is_url && std::path::Path::new(&expanded).exists();
        let body = if is_file {
            json!({ "kind": "file", "path": expanded, "label": label })
        } else {
            json!({ "kind": "url", "url": target, "label": label })
        };
        match post_json("/api/preview", body).await {
            Ok(v) => {
                if v["ok"].as_bool() == Some(true) {
                    format!(
                        "preview opened: {} ({})",
                        if is_file { &expanded } else { &target },
                        if is_file { "file" } else { "url" }
                    )
                } else {
                    format!("preview failed: {v}")
                }
            }
            Err(e) => format!("preview unavailable: {e}"),
        }
    }
}

pub struct FocusPaneTool;

#[async_trait]
impl Tool for FocusPaneTool {
    fn name(&self) -> &str {
        "focus_pane"
    }
    fn description(&self) -> &str {
        "Reveal and focus a pane in the veles-desktop window: chat, files, review, terminal, preview, or sessions."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pane": {"type": "string", "enum": ["chat", "files", "review", "terminal", "preview", "sessions"], "description": "Which pane to reveal"}
            },
            "required": ["pane"]
        })
    }
    async fn call(&self, args: Value) -> String {
        if let Some(out) = local_target("focus_pane", &args) {
            return out;
        }
        let pane = args["pane"].as_str().unwrap_or_default();
        if pane.is_empty() {
            return "error: pane is required".into();
        }
        match post_json("/api/focus", json!({ "pane": pane })).await {
            Ok(v) => {
                if v["ok"].as_bool() == Some(true) {
                    crate::i18n::lookup("focused pane: %{pane}", &[("pane", pane.to_string())])
                } else {
                    crate::i18n::lookup("focus failed: %{e}", &[("e", v.to_string())])
                }
            }
            Err(e) => format!("desktop unavailable: {e}"),
        }
    }
}

/// Evaluate Tcl in one of the two live interpreters.
///
/// This is the tool a Dev-tab session needs. Everything else the agent
/// can already do to the desktop's scripts is file-shaped — read, edit,
/// save — and none of it answers the question that matters when you are
/// working on a running window: *did that work*.
///
/// Two targets, and the difference is the whole point:
///
///   `gw`  — the gateway's plugin interpreter. Sandbox-ish: plugin
///           procs, hooks, the `veles::` bridge. Restarting the gateway
///           resets it.
///   `app` — the DESKTOP WINDOW ITSELF. Redefining a proc here changes
///           the running UI immediately, with no restart and no undo.
///           Refused unless the user has armed live patching in the Dev
///           pane, and every request is echoed into that pane's REPL
///           transcript so nothing happens invisibly.
pub struct TclEvalTool;

#[async_trait]
impl Tool for TclEvalTool {
    fn name(&self) -> &str {
        "tcl_eval"
    }
    fn description(&self) -> &str {
        "Evaluate Tcl in a live veles interpreter and return the result. target='gw' runs in the gateway's plugin interpreter (plugin procs, hooks, the veles:: bridge). target='app' runs in the veles-desktop window's OWN interpreter and MODIFIES THE RUNNING UI — redefining a proc takes effect at once, with no restart and no undo; it is refused unless the user has armed live patching in the Dev pane. Prefer editing the source file and re-sourcing it over one-off app evals, so the change survives a restart."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Tcl to evaluate"},
                "target": {
                    "type": "string",
                    "enum": ["gw", "app"],
                    "description": "gw = gateway plugin interpreter (default); app = the running desktop window"
                },
                "note": {
                    "type": "string",
                    "description": "One line on why, shown to the user in the Dev pane transcript"
                }
            },
            "required": ["code"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let code = args["code"].as_str().unwrap_or("").trim();
        if code.is_empty() {
            return "tcl_eval: no code supplied".into();
        }
        let target = args["target"].as_str().unwrap_or("gw");
        let body = json!({
            "code": code,
            "target": target,
            "note": args["note"].as_str().unwrap_or(""),
        });
        match post_json("/api/desktop/eval", body).await {
            Err(e) => format!("tcl_eval: {e}"),
            Ok(resp) => {
                let result = resp["result"].as_str().unwrap_or("").to_string();
                if resp["ok"].as_bool().unwrap_or(false) {
                    if result.is_empty() {
                        "ok (no value)".into()
                    } else {
                        result
                    }
                } else {
                    // The refusals are actionable — "not armed" tells the
                    // agent to ask the user rather than retry — so pass
                    // them through rather than flattening to "failed".
                    format!("tcl_eval failed ({target}): {result}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    /// The pane tools consult the process-local target BEFORE the
    /// loopback POST, so a surface that owns the window (the TUI) can
    /// move its own panes instead of erroring "desktop not running".
    /// Mutation: deleting the `local_target(...)` check makes this test
    /// red (the call would try the loopback instead of the target).
    #[tokio::test]
    async fn the_pane_tools_route_in_process_then_fall_back() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let cap = seen.clone();
        set_local_pane_target(move |name, _args| {
            *cap.lock().unwrap() = name.to_string();
            Some(format!("handled {name} in-process"))
        });

        assert_eq!(
            FocusPaneTool.call(json!({ "pane": "review" })).await,
            "handled focus_pane in-process"
        );
        assert_eq!(&*seen.lock().unwrap(), "focus_pane");

        assert_eq!(
            OpenPreviewTool
                .call(json!({ "url": "https://example.com" }))
                .await,
            "handled open_preview in-process"
        );
        assert_eq!(&*seen.lock().unwrap(), "open_preview");

        clear_local_pane_target();
    }
}
