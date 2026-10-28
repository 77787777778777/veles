//! computer_use tool — cua-driver bridge for desktop automation.
//!
//! #27 in the feature matrix. Wraps the cua-driver desktop automation
//! API. Provides capture (screenshot + AX tree), click, type, scroll,
//! key press, drag, and application listing — the same surface hermes'
//! computer_use offers. The cua-driver runs as a local HTTP server;
//! we send JSON-RPC-style requests to it.
//!
//! This implementation uses the cua-driver REST API (localhost:9876 or
//! whatever CUADRIER_URL is set to). If cua-driver isn't running, the
//! capture action returns a helpful error. The tool does NOT require
//! cua-driver at compile time — it's a pure HTTP client.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::Tool;

// ── computer_use ─────────────────────────────────────────────────────

pub struct ComputerUseTool {
    /// The session calling, for its rights: on a phone only the owner
    /// drives ([`may_drive`]).
    pub session_id: String,
}

/// May this principal drive the device the driver runs on?
///
/// On a phone the driver is veles' own accessibility service
/// (android/java/…/AccessibilityDriver.java), and the plan's rule is that
/// only the owner drives a person's phone (PLAN-sessions-and-phone.md A3,
/// USER decision D4). Elsewhere the ACL's `computer-use` capability
/// decides, as it always has. `None` is the owner's own surface.
pub fn may_drive(on_phone: bool, principal: Option<&crate::acl::Principal>) -> bool {
    if !on_phone {
        return true;
    }
    match principal {
        None => true,
        Some(p) => p.role == crate::acl::Role::Owner,
    }
}

/// Is the driver on this machine's loopback? Only then does the tool send
/// the gateway's bearer token, which the phone's driver requires.
fn loopback(url: &str) -> bool {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    let host = match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => rest.split(['/', ':']).next().unwrap_or(""),
    };
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// The capture's element list, cut to `max` bytes. A phone's labels are
/// rarely ASCII, so the cut falls on a character, never inside one.
fn clip(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}\n[...truncated]", &text[..text.floor_char_boundary(max)])
}

#[async_trait]
impl Tool for ComputerUseTool {
    fn name(&self) -> &str {
        "computer_use"
    }
    fn description(&self) -> &str {
        "Drive the desktop in the background: capture screenshots, click, type, scroll, press keys, drag. Requires cua-driver running locally — on a phone, veles' own accessibility driver, once the person turns it on (keys there: back, home, recents, notifications, quick_settings, enter; right_click is a long press). Actions: capture, click, double_click, right_click, middle_click, scroll, type, key, drag, list_apps, list_windows, focus_app, wait."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["capture", "click", "double_click", "right_click",
                             "middle_click", "scroll", "type", "key", "drag",
                             "list_apps", "list_windows", "focus_app", "wait"],
                    "description": "Which action to perform"
                },
                "mode": {
                    "type": "string",
                    "enum": ["som", "vision", "ax"],
                    "description": "Capture mode: som (screenshot with numbered overlays + AX tree), vision (plain screenshot), ax (AX tree only). Default: som"
                },
                "element": {
                    "type": "integer",
                    "description": "1-based SOM element index (from capture) to click/drag/etc"
                },
                "coordinate": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Pixel coordinates [x, y] relative to the window screenshot"
                },
                "text": {
                    "type": "string",
                    "description": "Text to type (for action=type)"
                },
                "keys": {
                    "type": "string",
                    "description": "Key combo for action=key (e.g. 'cmd+s', 'ctrl+alt+t', 'return', 'escape')"
                },
                "direction": {
                    "type": "string",
                    "enum": ["up", "down", "left", "right"],
                    "description": "Scroll direction"
                },
                "amount": {"type": "integer", "description": "Scroll ticks (default 3)"},
                "button": {
                    "type": "string",
                    "enum": ["left", "right", "middle"],
                    "description": "Mouse button (default left)"
                },
                "app": {
                    "type": "string",
                    "description": "App name for focus_app/list_apps filtering"
                },
                "seconds": {"type": "number", "description": "Seconds to wait (action=wait, max 30)"},
                "from_element": {"type": "integer", "description": "Source element for drag"},
                "to_element": {"type": "integer", "description": "Target element for drag"},
                "from_coordinate": {
                    "type": "array", "items": {"type": "integer"},
                    "description": "Source [x,y] for drag"
                },
                "to_coordinate": {
                    "type": "array", "items": {"type": "integer"},
                    "description": "Target [x,y] for drag"
                },
                "capture_after": {
                    "type": "boolean",
                    "description": "If true, re-capture after the action and include in response"
                }
            },
            "required": ["action"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let action = args["action"].as_str().unwrap_or_default().to_string();
        if action.is_empty() {
            return "error: action is required".into();
        }

        let on_phone = cfg!(target_os = "android");
        if !may_drive(
            on_phone,
            crate::acl::session_principal(&self.session_id).as_deref(),
        ) {
            return "error: only the phone's owner may drive it; this session is not the owner's"
                .into();
        }

        let url =
            std::env::var("CUA_DRIVER_URL").unwrap_or_else(|_| "http://127.0.0.1:9876".to_string());

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        // cua-driver exposes a REST endpoint per action
        let endpoint = format!("{url}/action");
        let mut req = client.post(&endpoint).json(&args);
        if loopback(&url) {
            let token = crate::authn::desktop_bearer_token();
            if !token.trim().is_empty() {
                req = req.bearer_auth(token.trim());
            }
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return if on_phone {
                    format!(
                        "computer_use error: the phone's driver is off ({e}). The person turns it \
                         on in veles' Phone page (Using the phone, for the agent), in Android's \
                         accessibility settings."
                    )
                } else {
                    format!(
                        "computer_use error: cannot reach cua-driver at {url}. \
                         Is it running? ({e})\n\
                         Start it with: cua-driver serve (or hermes computer-use doctor)"
                    )
                };
            }
        };

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            // Try to parse the error JSON
            if let Ok(err) = serde_json::from_str::<Value>(&body) {
                if let Some(msg) = err["error"].as_str() {
                    return format!("computer_use error: {msg}");
                }
            }
            return format!("computer_use error: HTTP {status}\n{body}");
        }

        // Parse the response — cua-driver returns JSON with screenshot
        // (base64), elements (AX tree), or a confirmation.
        if let Ok(json) = serde_json::from_str::<Value>(&body) {
            // For capture actions, include the element summary but not
            // the raw base64 screenshot (too large for text context).
            if action == "capture"
                || args.get("capture_after").and_then(|v| v.as_bool()) == Some(true)
            {
                let mode = args["mode"].as_str().unwrap_or("som");
                let elements = json
                    .get("elements")
                    .and_then(|e| e.as_array())
                    .map(|arr| arr.len())
                    .unwrap_or(0);
                let screenshot_path = json
                    .get("screenshot_path")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let mut summary =
                    format!("capture (mode={mode}): {elements} interactable elements found");
                if !screenshot_path.is_empty() {
                    summary.push_str(&format!("\nscreenshot saved: {screenshot_path}"));
                }
                // Include a truncated AX tree text if available
                if let Some(text) = json.get("ax_text").and_then(|t| t.as_str()) {
                    summary.push_str(&format!("\n{}", clip(text, 8000)));
                }
                summary
            } else {
                // For other actions, return the result field
                json.get("result")
                    .and_then(|r| r.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "ok".to_string())
            }
        } else {
            body
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_computer_use_requires_action() {
        let tool = ComputerUseTool {
            session_id: "t".into(),
        };
        let result = tool.call(serde_json::json!({})).await;
        assert!(result.contains("action is required"));
    }

    #[tokio::test]
    async fn test_computer_use_graceful_no_driver() {
        // With no cua-driver running on the test port, should get a
        // helpful error, not a panic.
        // Deliberately NOT taking testenv::lock(): this test awaits, and
        // holding a std MutexGuard across an await is the exact hazard
        // being removed elsewhere. CUA_DRIVER_URL is touched by nothing
        // else, so there is nothing to serialize against.
        std::env::set_var("CUA_DRIVER_URL", "http://127.0.0.1:59999");
        let tool = ComputerUseTool {
            session_id: "t".into(),
        };
        let result = tool.call(serde_json::json!({"action": "capture"})).await;
        assert!(result.contains("cannot reach cua-driver") || result.contains("error"));
        std::env::remove_var("CUA_DRIVER_URL");
    }

    /// On a phone only the owner drives (USER decision D4); elsewhere the
    /// ACL's capability decides, as before.
    #[test]
    fn only_the_owner_drives_a_phone() {
        use crate::acl::{Principal, Role};
        let as_role = |role: Role| Principal {
            role,
            ..Principal::resolve(None, &crate::acl::AclConfig::default())
        };
        assert!(may_drive(true, None), "the owner's own surface");
        assert!(may_drive(true, Some(&as_role(Role::Owner))));
        for r in [Role::Admin, Role::Trusted, Role::User, Role::Guest] {
            assert!(!may_drive(true, Some(&as_role(r))), "{r:?} on a phone");
            assert!(
                may_drive(false, Some(&as_role(r))),
                "{r:?} elsewhere: the ACL decides"
            );
        }
    }

    #[test]
    fn the_token_goes_only_to_loopback() {
        for u in [
            "http://127.0.0.1:9876",
            "http://localhost:9876/x",
            "127.0.0.1:9876",
            "http://[::1]:9876",
        ] {
            assert!(loopback(u), "{u}");
        }
        for u in [
            "http://10.0.30.2:9876",
            "https://example.org",
            "http://127.0.0.1.example.org",
        ] {
            assert!(!loopback(u), "{u}");
        }
    }

    /// A Hindi phone's capture: three bytes a character, and a cut at a
    /// byte count that falls inside one panicked the tool.
    #[test]
    fn a_long_capture_is_cut_on_a_character() {
        let text = "[1] LinearLayout \"वाई-फ़ाई\"\n".repeat(400);
        for max in [8000, 8001, 8002] {
            let c = clip(&text, max);
            assert!(c.ends_with("[...truncated]"), "{max}");
            assert!(c.len() <= max + "\n[...truncated]".len(), "{max}");
        }
        assert_eq!(clip("short", 8000), "short");
    }
}
