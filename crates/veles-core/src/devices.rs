//! Devices — what a connected frontend can do for the agent
//! (PLAN-sessions-and-phone.md A0; USER decision D3).
//!
//! A frontend — the Tk desktop on a laptop, the same desktop on a phone
//! (AndroWish), later the phone's gateway service in Java — connects to
//! the gateway it belongs to and says what it can do: each capability a
//! name, a description and a JSON schema for its arguments. The agent's
//! `device` tool lists them and calls them; a call is pushed to the one
//! client that offers it, which runs it and posts the answer back.
//!
//! The shape is `/api/desktop/eval`'s (a stream of requests out, a POST
//! of results in, a parked oneshot per request), generalised: requests
//! are TARGETED at one client instead of broadcast to every window, the
//! payload is an action and JSON arguments instead of Tcl, and the
//! catalog is the clients' own, so a phone with a camera and a laptop
//! without one advertise different things and the agent sees which.
//!
//! The registry lives in the gateway process (the desktop API serves the
//! routes); the tool reaches it over loopback HTTP from whichever process
//! the session runs in, as `tcl_eval` does.

use crate::lockutil::LockOk;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a call waits for its answer unless the capability says.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);
/// The longest any capability may ask for (a photo, speech).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(180);

/// One thing a client can do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub name: String,
    pub description: String,
    /// JSON schema of `args`; `{}` = none.
    #[serde(default)]
    pub schema: Value,
    /// It works only while the client's app is on the screen (a phone
    /// freezes a backgrounded app; a camera needs a window).
    #[serde(default)]
    pub foreground: bool,
    /// Seconds to wait for the answer; 0 = the default.
    #[serde(default)]
    pub timeout_secs: u64,
}

/// What a client says when it connects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    /// Unique per running client (minted by it, stable across its own
    /// reconnects).
    pub client_id: String,
    /// The name the agent uses for it: "phone", "laptop". Several
    /// clients may share one (two windows on one laptop).
    pub name: String,
    /// android | linux | macos | windows
    pub platform: String,
    pub capabilities: Vec<Capability>,
}

/// A request pushed to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub action: String,
    pub args: Value,
}

struct Client {
    hello: Hello,
    /// The open request stream, when the client is listening.
    tx: Option<tokio::sync::mpsc::UnboundedSender<Request>>,
    seen: Instant,
}

#[derive(Default)]
struct Registry {
    clients: HashMap<String, Client>,
    pending: HashMap<String, tokio::sync::oneshot::Sender<Answer>>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

/// A client's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub ok: bool,
    pub result: Value,
}

/// Register (or refresh) what a client offers.
pub fn hello(h: Hello) {
    let mut r = registry().lock_ok();
    let tx = r.clients.remove(&h.client_id).and_then(|c| c.tx);
    r.clients.insert(
        h.client_id.clone(),
        Client {
            hello: h,
            tx,
            seen: Instant::now(),
        },
    );
}

/// The client opened its request stream. A client not seen before gets
/// a placeholder entry its hello fills in.
pub fn attach(client_id: &str) -> tokio::sync::mpsc::UnboundedReceiver<Request> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut r = registry().lock_ok();
    let c = r
        .clients
        .entry(client_id.to_string())
        .or_insert_with(|| Client {
            hello: Hello {
                client_id: client_id.to_string(),
                name: String::new(),
                platform: String::new(),
                capabilities: Vec::new(),
            },
            tx: None,
            seen: Instant::now(),
        });
    c.tx = Some(tx);
    c.seen = Instant::now();
    rx
}

/// The stream closed: the client is gone until it reconnects.
pub fn detach(client_id: &str) {
    if let Some(c) = registry().lock_ok().clients.get_mut(client_id) {
        c.tx = None;
    }
}

/// A client's answer to request `id`. `false` = nothing waited for it
/// (it timed out, or never existed).
pub fn answer(id: &str, a: Answer) -> bool {
    match registry().lock_ok().pending.remove(id) {
        Some(tx) => tx.send(a).is_ok(),
        None => false,
    }
}

/// One connected device, as the agent sees it.
#[derive(Debug, Clone, Serialize)]
pub struct Listed {
    pub name: String,
    pub platform: String,
    pub client_id: String,
    pub connected: bool,
    pub capabilities: Vec<Capability>,
}

/// Every client that has said hello, connected or not.
pub fn catalog() -> Vec<Listed> {
    let r = registry().lock_ok();
    let mut out: Vec<Listed> = r
        .clients
        .values()
        .filter(|c| !c.hello.name.is_empty())
        .map(|c| Listed {
            name: c.hello.name.clone(),
            platform: c.hello.platform.clone(),
            client_id: c.hello.client_id.clone(),
            connected: c.tx.as_ref().is_some_and(|t| !t.is_closed()),
            capabilities: c.hello.capabilities.clone(),
        })
        .collect();
    out.sort_by(|a, b| (&a.name, &a.client_id).cmp(&(&b.name, &b.client_id)));
    out
}

/// Pick the client for a call: the named device (by name or client id)
/// that offers `action`, connected ones first, most recently seen first.
fn pick(r: &Registry, device: Option<&str>, action: &str) -> Result<(String, Capability), String> {
    let mut offering: Vec<&Client> = r
        .clients
        .values()
        .filter(|c| c.hello.capabilities.iter().any(|k| k.name == action))
        .filter(|c| match device {
            Some(d) => c.hello.name == d || c.hello.client_id == d,
            None => true,
        })
        .collect();
    if offering.is_empty() {
        let names: Vec<String> = r
            .clients
            .values()
            .filter(|c| !c.hello.name.is_empty())
            .map(|c| c.hello.name.clone())
            .collect();
        return Err(match device {
            Some(d) if !names.iter().any(|n| n == d) => format!(
                "no device named {d:?} is connected (connected: {})",
                if names.is_empty() {
                    "none".into()
                } else {
                    names.join(", ")
                }
            ),
            Some(d) => format!("device {d:?} cannot {action} — action=list shows what it can do"),
            None => {
                format!("no connected device can {action} — action=list shows what each can do")
            }
        });
    }
    // Several devices of different names offer it and none was named:
    // the agent must say which (a photo from the laptop is not a photo
    // from the phone).
    if device.is_none() {
        let mut names: Vec<&str> = offering.iter().map(|c| c.hello.name.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        if names.len() > 1 {
            return Err(format!(
                "several devices can {action} ({}): say which with device=",
                names.join(", ")
            ));
        }
    }
    offering.sort_by_key(|c| {
        (
            !c.tx.as_ref().is_some_and(|t| !t.is_closed()),
            std::cmp::Reverse(c.seen),
        )
    });
    let c = offering[0];
    let cap = c
        .hello
        .capabilities
        .iter()
        .find(|k| k.name == action)
        .cloned()
        .expect("filtered on it");
    Ok((c.hello.client_id.clone(), cap))
}

/// Run `action` on a device and wait for its answer.
pub async fn call(device: Option<&str>, action: &str, args: Value) -> Result<Value, String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let (rx, timeout, name) = {
        let mut r = registry().lock_ok();
        let (client_id, cap) = pick(&r, device, action)?;
        let c = r.clients.get(&client_id).expect("picked");
        let name = c.hello.name.clone();
        let Some(tx) = c.tx.clone().filter(|t| !t.is_closed()) else {
            return Err(format!(
                "{name} is not connected right now{}",
                if c.hello.platform == "android" {
                    " — open veles on the phone"
                } else {
                    ""
                }
            ));
        };
        let (otx, orx) = tokio::sync::oneshot::channel();
        r.pending.insert(id.clone(), otx);
        if tx
            .send(Request {
                id: id.clone(),
                action: action.to_string(),
                args,
            })
            .is_err()
        {
            r.pending.remove(&id);
            return Err(format!("{name} disconnected"));
        }
        let t = match cap.timeout_secs {
            0 => DEFAULT_TIMEOUT,
            s => Duration::from_secs(s).min(MAX_TIMEOUT),
        };
        (orx, t, name)
    };
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(a)) if a.ok => Ok(a.result),
        Ok(Ok(a)) => Err(match a.result {
            Value::String(s) => s,
            v => v.to_string(),
        }),
        Ok(Err(_)) => Err(format!("{name} dropped the request")),
        Err(_) => {
            registry().lock_ok().pending.remove(&id);
            Err(format!(
                "{name} did not answer within {}s{}",
                timeout.as_secs(),
                " (a phone in the background answers nothing that needs the screen)"
            ))
        }
    }
}

/// The catalog as the agent reads it (`device action=list`).
pub fn catalog_json() -> Value {
    json!(catalog())
}

// ── the agent's tool ─────────────────────────────────────────────────

/// `device`: list what the connected devices offer, and call one action.
/// Deferred, owner-only (`acl::tool_cap`): a phone's location, its
/// notifications, its screen are the owner's.
pub struct DeviceTool;

#[async_trait::async_trait]
impl crate::tools::Tool for DeviceTool {
    fn name(&self) -> &str {
        "device"
    }

    fn description(&self) -> &str {
        "Use a device the user has connected to veles — their phone (veles open on it) or a desktop \
         window: post a notification, speak aloud, vibrate, open a link, a map or the dialer, share \
         text, find where the phone is, and whatever else each device offers. action=list shows \
         every connected device and its actions with their arguments; then call one with \
         action=<name> and args={…}, and device=<name> when more than one device can do it. A phone \
         answers actions marked foreground only while veles is on its screen."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "list, or an action a device offers"},
                "device": {"type": "string", "description": "which device (its name from list), when several could"},
                "args": {"type": "object", "description": "the action's arguments, as list describes them"}
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Value) -> String {
        let Some(action) = args["action"].as_str() else {
            return "error: device needs an action (list shows them)".to_string();
        };
        if action == "list" {
            return match crate::desktop_tools::desktop_get("/api/device/list").await {
                Ok(v) => render_catalog(&v),
                Err(e) => format!("error: {e}"),
            };
        }
        let body = json!({
            "device": args["device"].as_str(),
            "action": action,
            "args": if args["args"].is_object() { args["args"].clone() } else { json!({}) },
        });
        match crate::desktop_tools::desktop_post(
            "/api/device/call",
            body,
            MAX_TIMEOUT + Duration::from_secs(10),
        )
        .await
        {
            Ok(v) if v["ok"].as_bool() == Some(true) => match &v["result"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            },
            Ok(v) => format!(
                "error: {}",
                v["error"].as_str().unwrap_or("the device refused")
            ),
            Err(e) => format!("error: {e}"),
        }
    }
}

/// The catalog as the model reads it: one block per device, one line per
/// action with its arguments. Clients that share a name are one device to
/// the agent: a phone is its window (the foreground actions) and its
/// gateway service (the background ones), and `device=phone` reaches
/// either.
fn render_catalog(v: &Value) -> String {
    let Some(list) = v.as_array().filter(|l| !l.is_empty()) else {
        return "No devices are connected. A phone connects while veles is open on it; a desktop \
                window connects while it runs."
            .to_string();
    };
    // name → (platform, any client connected, actions by name)
    let mut devices: Vec<(String, String, bool, Vec<Value>)> = Vec::new();
    for d in list {
        let name = d["name"].as_str().unwrap_or("?").to_string();
        let platform = d["platform"].as_str().unwrap_or("?").to_string();
        let connected = d["connected"].as_bool() == Some(true);
        let i = match devices.iter().position(|x| x.0 == name && x.1 == platform) {
            Some(i) => i,
            None => {
                devices.push((name, platform, false, Vec::new()));
                devices.len() - 1
            }
        };
        devices[i].2 |= connected;
        for c in d["capabilities"].as_array().into_iter().flatten() {
            if !devices[i].3.iter().any(|k| k["name"] == c["name"]) {
                devices[i].3.push(c.clone());
            }
        }
    }
    let mut out = String::new();
    for (name, platform, connected, mut caps) in devices {
        caps.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        out.push_str(&format!(
            "{name} ({platform}{}):\n",
            if connected { "" } else { ", not connected now" }
        ));
        for c in &caps {
            let args = c["schema"]["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {}{}: {}{}\n",
                c["name"].as_str().unwrap_or("?"),
                if args.is_empty() {
                    String::new()
                } else {
                    format!("({args})")
                },
                c["description"].as_str().unwrap_or(""),
                if c["foreground"].as_bool() == Some(true) {
                    " [foreground]"
                } else {
                    ""
                }
            ));
        }
    }
    out.push_str("Call one with action=<name>, args={…}, device=<name>.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str) -> Capability {
        Capability {
            name: name.into(),
            description: format!("{name} it"),
            schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
            foreground: false,
            timeout_secs: 2,
        }
    }

    fn hello_as(client: &str, name: &str, caps: &[&str]) {
        hello(Hello {
            client_id: client.into(),
            name: name.into(),
            platform: "android".into(),
            capabilities: caps.iter().map(|c| cap(c)).collect(),
        });
    }

    /// The registry is process-global: each test uses its own names.
    #[tokio::test]
    async fn a_call_reaches_the_one_client_that_offers_it_and_its_answer_returns() {
        hello_as("dev-a1", "dev-phone-a", &["speak", "notify"]);
        hello_as("dev-a2", "dev-laptop-a", &["notify"]);
        let mut rx1 = attach("dev-a1");
        let _rx2 = attach("dev-a2");
        let answered = crate::rt::spawn(async move {
            let req = rx1.recv().await.expect("the phone gets the call");
            assert_eq!(req.action, "speak");
            assert_eq!(req.args["text"], "hi");
            answer(
                &req.id,
                Answer {
                    ok: true,
                    result: json!("speaking"),
                },
            )
        });
        let v = call(Some("dev-phone-a"), "speak", json!({"text": "hi"}))
            .await
            .unwrap();
        assert_eq!(v, json!("speaking"));
        assert!(answered.await.unwrap(), "the answer found its waiter");
    }

    #[tokio::test]
    async fn ambiguity_absence_and_silence_are_named() {
        hello_as("dev-b1", "dev-phone-b", &["notify-b"]);
        hello_as("dev-b2", "dev-laptop-b", &["notify-b"]);
        let _r1 = attach("dev-b1");
        let _r2 = attach("dev-b2");
        let e = call(None, "notify-b", json!({})).await.unwrap_err();
        assert!(e.contains("say which"), "{e}");
        let e = call(Some("dev-nowhere"), "notify-b", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("no device named"), "{e}");
        let e = call(Some("dev-phone-b"), "fly-b", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("cannot fly-b"), "{e}");
        // Offered and connected, but nobody answers: the timeout says so.
        let e = call(Some("dev-phone-b"), "notify-b", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("did not answer within 2s"), "{e}");
        // Not listening at all: said at once, not after a timeout.
        hello_as("dev-b3", "dev-tablet-b", &["notify-b3"]);
        let e = call(Some("dev-tablet-b"), "notify-b3", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("not connected"), "{e}");
        // A stream that died without its detach (the receiver dropped):
        // still not connected, said at once.
        hello_as("dev-b4", "dev-watch-b", &["notify-b4"]);
        drop(attach("dev-b4"));
        let e = call(Some("dev-watch-b"), "notify-b4", json!({}))
            .await
            .unwrap_err();
        assert!(e.contains("not connected"), "{e}");
    }

    #[test]
    fn a_late_answer_finds_no_waiter_and_says_so() {
        // A late answer finds no waiter, and says so.
        assert!(!answer(
            "dev-no-such-request",
            Answer {
                ok: true,
                result: json!(1)
            }
        ));
    }

    #[test]
    fn the_catalog_reads_as_lines_the_model_can_act_on() {
        let v = json!([{
            "name": "phone", "platform": "android", "client_id": "x", "connected": true,
            "capabilities": [{"name": "speak", "description": "Read text aloud.",
                "schema": {"type":"object","properties":{"text":{"type":"string"}}},
                "foreground": false, "timeout_secs": 0}]
        }]);
        let s = render_catalog(&v);
        assert!(s.contains("phone (android):"), "{s}");
        assert!(s.contains("- speak(text): Read text aloud."), "{s}");
        assert!(render_catalog(&json!([])).contains("No devices are connected"));

        // A phone is two clients — its window and its gateway service —
        // and one device to the agent.
        let cap = |n: &str| json!({"name": n, "description": n, "schema": {}, "foreground": false});
        let v = json!([
            {"name": "phone", "platform": "android", "client_id": "w", "connected": false,
             "capabilities": [cap("speak"), cap("toast")]},
            {"name": "phone", "platform": "android", "client_id": "phone-service",
             "connected": true, "capabilities": [cap("battery"), cap("notifications")]},
            {"name": "laptop", "platform": "linux", "client_id": "l", "connected": true,
             "capabilities": [cap("speak")]},
        ]);
        let s = render_catalog(&v);
        assert_eq!(s.matches("phone (android").count(), 1, "{s}");
        assert!(
            s.contains("phone (android):\n- battery"),
            "connected through either: {s}"
        );
        for a in ["- notifications", "- speak", "- toast"] {
            assert!(s.split("laptop").next().unwrap().contains(a), "{a}: {s}");
        }
        assert!(s.contains("laptop (linux):\n- speak"), "{s}");
    }
}
