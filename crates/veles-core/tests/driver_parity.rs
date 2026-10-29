// A test/example driver: it spawns onto a scratch runtime it owns;
// the M3 hop (clippy.toml) is for production core/frontends code.
#![allow(clippy::disallowed_methods)]

//! One wire, two implementations, one expected answer.
//!
//! The question this file exists to settle: can a COMPLETE provider
//! driver — a wire the engine did not previously speak, with its own
//! low-level translation in both directions — be written in Tcl AND in
//! out-of-tree Rust?
//!
//! Asserting each driver separately would prove they both run. It would
//! not prove they AGREE, and a plugin system whose two languages are
//! subtly different capabilities is the thing being tested. So one
//! canned Anthropic Messages response goes to both, and the two
//! `Completion` values must be equal — content, reasoning, tool calls,
//! and all four token counters.
//!
//! The fixture also captures what each driver SENT, so the outbound
//! half of the translation is checked too: the system prompt hoisted
//! out of `messages`, tool results turned into `tool_result` blocks in
//! a user message, and the `x-api-key` / `anthropic-version` headers a
//! driver could not send at all before `http_post` grew a headers
//! argument.
//!
//! The `.so` is a separate workspace member with no dependency edge, so
//! a bare `cargo test` SKIPS with a loud message when it is missing;
//! `scripts/precommit.sh` and the nix `preCheck` build it and set
//! `VELES_REQUIRE_PLUGIN_SO=1`, which turns that skip into a failure.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use veles_core::providers::{
    CacheMode, ChatMessage, ChatRequest, PluginRoute, RouteClient, ToolMeta,
};

/// The cdylib's file name on this platform: `libveles_plugin_anthropic_so.so`
/// on Linux, `veles_plugin_anthropic_so.dll` on Windows. The hard-coded Unix
/// name found nothing on Windows, so `both_drivers()` skipped before loading
/// EITHER driver and every case then died in `available_drivers()`.
fn so_name() -> String {
    format!(
        "{}veles_plugin_anthropic_so{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    )
}

/// The one canned answer both drivers translate. Every branch of the
/// response half is represented: text, extended thinking, a tool call
/// whose arguments are an OBJECT on this wire and a STRING on ours, and
/// cache accounting under Anthropic's spellings.
const ANTHROPIC_RESPONSE: &str = r#"{
  "id": "msg_01",
  "type": "message",
  "role": "assistant",
  "model": "claude-x",
  "content": [
    { "type": "thinking", "thinking": "weighing it up" },
    { "type": "text", "text": "Here is the answer." },
    { "type": "tool_use", "id": "toolu_9", "name": "search",
      "input": { "q": "veles", "limit": 3 } }
  ],
  "usage": {
    "input_tokens": 101,
    "output_tokens": 42,
    "cache_creation_input_tokens": 5,
    "cache_read_input_tokens": 90
  }
}"#;

fn find_named(dir: &Path, want: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let mut subdirs = Vec::new();
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_file() && p.file_name().is_some_and(|n| n == want) {
            return Some(p);
        }
        if p.is_dir() {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if name != "deps" && name != "build" && name != "incremental" && !name.starts_with('.')
            {
                subdirs.push(p);
            }
        }
    }
    subdirs
        .into_iter()
        .find_map(|d| find_named(&d, want, depth - 1))
}

fn so_path() -> Option<PathBuf> {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target = crate_dir.parent()?.parent()?.join("target");
    find_named(&target, &so_name(), 4)
}

fn required() -> bool {
    std::env::var("VELES_REQUIRE_PLUGIN_SO")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

/// A HTTP server that answers every request with the fixture and keeps the
/// raw requests it was sent.
///
/// It accepts indefinitely rather than a counted number of connections, and
/// that is deliberate. Counting forces every test to know exactly how many
/// requests a driver will make — and "how many" is a property of the DRIVER,
/// which changes as the driver grows (consulting the capability catalog added
/// a `GET /v1/models` in front of the `POST`). A server sized for one
/// connection then closes early and the second driver gets a transport error
/// that looks like a driver bug; sized generously, it hangs the test. Two
/// rounds of that is enough: the loop runs until the test's runtime is
/// dropped, which is when the test is over.
async fn fixture_server(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            // Read headers, then exactly Content-Length bytes: a
            // driver's POST body is what half of this test is about.
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let mut need: Option<usize> = None;
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                raw.extend_from_slice(&buf[..n]);
                if need.is_none() {
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                    {
                        let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        need = Some(end + len);
                    }
                }
                if need.is_some_and(|n| raw.len() >= n) {
                    break;
                }
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&raw).to_string());
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// A fixture endpoint that ROUTES, because a driver that asks its endpoint
/// what it supports makes more than one kind of call.
///
/// `GET /v1/models` answers `catalog`; everything else answers `answer`,
/// either as a JSON body or — when `sse` is set — as server-sent events.
/// `status` is answered on the non-catalog path, which is how the error
/// classification gets tested against a real HTTP status rather than a
/// driver's say-so.
///
/// Unlike `fixture_server` this reads a request with NO Content-Length by
/// stopping at the end of the headers: a GET has no body, and waiting for
/// one would hang on a keep-alive connection. It also accepts indefinitely,
/// for the reason given on `fixture_server` — the number of calls a driver
/// makes is the driver's business, not the test's.
async fn route_server(
    catalog: &'static str,
    answer: &'static str,
    sse: bool,
    status: u16,
) -> (String, Arc<Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 8192];
            let mut need: Option<usize> = None;
            loop {
                let n = match sock.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                raw.extend_from_slice(&buf[..n]);
                if need.is_none() {
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
                    {
                        let head = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            // A GET carries no body. Treating "no
                            // Content-Length" as "nothing more to read" is
                            // what keeps the catalog request from hanging.
                            .unwrap_or(0);
                        need = Some(end + len);
                    }
                }
                if need.is_some_and(|n| raw.len() >= n) {
                    break;
                }
            }
            let text = String::from_utf8_lossy(&raw).to_string();
            captured.lock().unwrap().push(text.clone());

            let is_catalog = text.starts_with("GET /v1/models");
            let (body, code) = if is_catalog {
                (catalog.to_string(), 200u16)
            } else {
                (answer.to_string(), status)
            };
            let ct = if sse && !is_catalog {
                "text/event-stream"
            } else {
                "application/json"
            };
            let reason = match code {
                200 => "OK",
                429 => "Too Many Requests",
                _ => "Error",
            };
            let head = format!(
                "HTTP/1.1 {code} {reason}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("http://{addr}"), seen)
}

/// The request both drivers translate. Deliberately awkward: a system
/// message that has to be HOISTED, an assistant turn carrying a tool
/// call, and a tool RESULT that has to become a block in a user
/// message — the three places the two wires disagree about structure.
fn request(model: &str) -> ChatRequest {
    let mut assistant = ChatMessage {
        role: "assistant".into(),
        content: "Let me look.".into(),
        reasoning: None,
        display_reasoning: None,
        tool_calls: Some(serde_json::json!([{
            "id": "toolu_1", "type": "function",
            "function": { "name": "search", "arguments": "{\"q\":\"earlier\"}" }
        }])),
        tool_meta: None,
        attachments: Vec::new(),
        injection: None,
        msg_id: None,
    };
    assistant.display_reasoning = None;
    ChatRequest {
        model: model.to_string(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: "You are terse.".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: None,
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
            ChatMessage::user("what is veles"),
            assistant,
            ChatMessage {
                role: "tool".into(),
                content: "an agent".into(),
                reasoning: None,
                display_reasoning: None,
                tool_calls: None,
                tool_meta: Some(ToolMeta {
                    tool_call_id: "toolu_1".into(),
                    name: "search".into(),
                }),
                attachments: Vec::new(),
                injection: None,
                msg_id: None,
            },
        ],
        tools: vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "search",
                "description": "Look something up.",
                "parameters": { "type": "object", "properties": { "q": { "type": "string" } } }
            }
        })],
        reasoning_effort: None,
        stream: false,
        replay_reasoning: false,
        cache_mode: CacheMode::Off,
        extra_body: None,
    }
}

/// Everything about a `Completion` that a driver is responsible for.
#[derive(Debug, PartialEq, Eq)]
struct Shape {
    content: String,
    reasoning: Option<String>,
    tool_name: Option<String>,
    tool_args: Option<String>,
    prompt: Option<u64>,
    completion: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
}

fn shape(c: &veles_core::providers::Completion) -> Shape {
    let call = c
        .message
        .tool_calls
        .as_ref()
        .and_then(|v| v.get(0).cloned());
    Shape {
        content: c.message.content.clone(),
        reasoning: c.message.reasoning.clone(),
        tool_name: call
            .as_ref()
            .and_then(|v| v["function"]["name"].as_str().map(str::to_string)),
        tool_args: call
            .as_ref()
            .and_then(|v| v["function"]["arguments"].as_str().map(str::to_string)),
        prompt: c.prompt_tokens,
        completion: c.completion_tokens,
        cached: c.cached_tokens,
        cache_write: c.cache_write_tokens,
    }
}

async fn drive(driver: &str, base_url: &str, model: &str) -> veles_core::providers::Completion {
    RouteClient::Plugin(PluginRoute {
        driver: driver.to_string(),
        base_url: base_url.to_string(),
        api_key: "sk-fixture".into(),
        ..Default::default()
    })
    .chat(&request(model), "anthropic-under-test")
    .await
    .unwrap_or_else(|e| panic!("driver `{driver}` failed: {e}"))
}

/// Assert the OUTBOUND half: what the driver actually put on the wire.
fn assert_outbound(raw: &str, who: &str) {
    let lower = raw.to_lowercase();
    assert!(
        lower.contains("x-api-key: sk-fixture"),
        "{who}: the credential must reach the endpoint — a driver that cannot set \
         headers cannot authenticate. Request was:\n{raw}"
    );
    assert!(
        lower.contains("anthropic-version: 2023-06-01"),
        "{who}: the dated wire-version header is required by this API:\n{raw}"
    );
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("{who}: body was not JSON ({e}): {body}"));

    // The system prompt is HOISTED, not a message.
    assert_eq!(
        v["system"], "You are terse.",
        "{who}: system must be top-level"
    );
    for m in v["messages"].as_array().expect("messages") {
        assert_ne!(m["role"], "system", "{who}: no system message may survive");
        assert_ne!(m["role"], "tool", "{who}: this wire has no `tool` role");
    }
    // Anthropic requires max_tokens; OpenAI never sets one.
    assert!(
        v["max_tokens"].as_u64().is_some(),
        "{who}: max_tokens is required"
    );

    // The assistant's tool call became a tool_use BLOCK with an OBJECT
    // input, not a sibling array with a string.
    let assistant = v["messages"]
        .as_array()
        .and_then(|a| a.iter().find(|m| m["role"] == "assistant"))
        .unwrap_or_else(|| panic!("{who}: no assistant message in {body}"));
    let use_block = assistant["content"]
        .as_array()
        .and_then(|b| b.iter().find(|x| x["type"] == "tool_use"))
        .unwrap_or_else(|| panic!("{who}: no tool_use block in {body}"));
    assert_eq!(use_block["id"], "toolu_1", "{who}");
    assert_eq!(
        use_block["input"]["q"], "earlier",
        "{who}: arguments must be an object"
    );

    // The tool RESULT became a tool_result block in a USER message.
    let result_block = v["messages"]
        .as_array()
        .and_then(|a| {
            a.iter().find_map(|m| {
                m["content"]
                    .as_array()?
                    .iter()
                    .find(|b| b["type"] == "tool_result")
                    .map(|b| (m["role"].clone(), b.clone()))
            })
        })
        .unwrap_or_else(|| panic!("{who}: no tool_result block in {body}"));
    assert_eq!(
        result_block.0, "user",
        "{who}: a tool result rides a USER message"
    );
    assert_eq!(result_block.1["tool_use_id"], "toolu_1", "{who}");

    // Tools: name/description/input_schema at the top level.
    let tool = &v["tools"][0];
    assert_eq!(tool["name"], "search", "{who}");
    assert_eq!(tool["input_schema"]["type"], "object", "{who}");

    // The reserved key the engine adds is for the DRIVER, not the wire.
    assert!(
        v.get("veles").is_none(),
        "{who}: the engine's `veles` key must not be forwarded upstream:\n{body}"
    );
}

/// Load BOTH drivers under distinct names, once per test binary.
///
/// Extracted because every feature test below needs the same two
/// registrations, and because the setup has a hazard that makes copying it
/// per test actively wrong: `VELES_HOME` and `VELES_PLUGIN_DIRS` are
/// PROCESS env vars, and cargo runs tests in parallel. Two tests setting
/// them concurrently would spawn two runtimes against whichever home won the
/// race. The `OnceCell` makes the boot happen exactly once.
///
/// Registration itself is replace-by-name, so re-entering is harmless even
/// without the guard — but the env vars are not, and the guard covers both.
async fn both_drivers() {
    static LOADED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    LOADED
        .get_or_init(|| async {
            let Some(so) = so_path() else {
                let so_name = so_name();
                assert!(
                    !required(),
                    "VELES_REQUIRE_PLUGIN_SO is set but {so_name} is nowhere under target/ — \
                     build it: cargo build -p veles-plugin-anthropic-so"
                );
                eprintln!("SKIP: {so_name} not built");
                return;
            };

            // A scratch home, and the Tcl driver loaded through the REAL
            // boot path — VELES_PLUGIN_DIRS, `spawn`, `load_plugins`. Not a
            // proc eval'd into an interpreter by hand: the question is
            // whether a plugin on disk works.
            //
            // Leaked deliberately, via `keep()`: the runtime thread owns this
            // directory for the life of the test binary, and a TempDir that
            // dropped at the end of this function would pull the plugin tree
            // out from under a driver still being called by a later test.
            // `keep` (not the deprecated `into_path`) is the version that says
            // "persist this and hand me the path" without the deprecation
            // warning `-D warnings` turns into a build failure.
            let home_path = tempfile::TempDir::new().expect("tempdir").keep();
            std::env::set_var("VELES_HOME", &home_path);
            std::fs::write(home_path.join("config.toml"), "").expect("config");
            let examples = format!("{}/../../plugins/examples", env!("CARGO_MANIFEST_DIR"));
            std::env::set_var(
                "VELES_PLUGIN_DIRS",
                format!("{examples}/tcl-anthropic-driver"),
            );
            let tcl = veles_core::tclrt::spawn(home_path).expect("tcl spawn");
            // `spawn` returns before the thread has loaded plugins, so
            // asking for the driver now would find nothing — which is
            // exactly how this test failed before `wait_ready` existed, and
            // how `doctor --providers` was reporting an empty driver list.
            tokio::task::spawn_blocking({
                let tcl = tcl.clone();
                move || tcl.wait_ready()
            })
            .await
            .expect("join")
            .expect("the tcl runtime came up");

            // The Tcl driver registers under the shared name; rename it so
            // both can be resident at once and neither can be mistaken for
            // the other.
            let tcl_driver = veles_core::providers::driver("anthropic-messages")
                .expect("the Tcl plugin registered `anthropic-messages`");
            assert!(
                tcl_driver.plugin.starts_with("tcl:"),
                "and it must be attributed to Tcl, got {}",
                tcl_driver.plugin
            );
            veles_core::providers::register_driver(veles_core::providers::ProviderDriver {
                name: "anthropic-tcl".into(),
                plugin: tcl_driver.plugin.clone(),
                call: tcl_driver.call.clone(),
            })
            .expect("alias the tcl driver");

            // The Rust driver, loaded as a real out-of-tree .so.
            veles_core::rustplugins::load_so(&so, "anthropic-so", "test").expect("the .so loads");
            let so_driver = veles_core::providers::driver("anthropic-messages")
                .expect("the .so registered `anthropic-messages`");
            assert!(
                so_driver.plugin.starts_with("so:"),
                "the .so registration must replace the Tcl one under the shared name, got {}",
                so_driver.plugin
            );
            veles_core::providers::register_driver(veles_core::providers::ProviderDriver {
                name: "anthropic-rust".into(),
                plugin: so_driver.plugin.clone(),
                call: so_driver.call.clone(),
            })
            .expect("alias the rust driver");
        })
        .await;
}

/// Both driver names, so a test can loop over them and assert the same thing
/// of each. Parity is the point of the file; a feature test that exercised
/// only one implementation would let them drift silently.
const BOTH: &[&str] = &["anthropic-tcl", "anthropic-rust"];

/// Send `req` through one driver against one endpoint, with the endpoint
/// knobs set. Returns the completion, or the error — callers decide which
/// they are asserting on.
async fn drive_with(
    driver: &str,
    base_url: &str,
    knobs: veles_core::providers::PluginRoute,
    req: ChatRequest,
) -> Result<veles_core::providers::Completion, veles_core::providers::ProviderError> {
    let route = veles_core::providers::PluginRoute {
        driver: driver.to_string(),
        base_url: base_url.to_string(),
        api_key: "sk-fixture".into(),
        ..knobs
    };
    veles_core::providers::RouteClient::Plugin(route)
        .chat(&req, "anthropic-under-test")
        .await
}

/// A route with no endpoint knobs set — every driver default in force.
fn default_route() -> veles_core::providers::PluginRoute {
    veles_core::providers::PluginRoute::default()
}

#[tokio::test]
async fn the_same_driver_in_tcl_and_in_rust_agree_exactly() {
    both_drivers().await;

    // One fixture, two requests.
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    let from_tcl = drive("anthropic-tcl", &url, "claude-x").await;
    let from_rust = drive("anthropic-rust", &url, "claude-x").await;

    // ── THE assertion ────────────────────────────────────────────────
    assert_eq!(
        shape(&from_tcl),
        shape(&from_rust),
        "the Tcl driver and the Rust driver must produce the SAME Completion \
         from the same response — if they differ, the two plugin languages are \
         not the same capability"
    );

    // And it must be the RIGHT answer, not merely the same wrong one.
    let s = shape(&from_tcl);
    assert_eq!(s.content, "Here is the answer.");
    assert_eq!(s.reasoning.as_deref(), Some("weighing it up"));
    assert_eq!(s.tool_name.as_deref(), Some("search"));
    let args: serde_json::Value =
        serde_json::from_str(s.tool_args.as_deref().unwrap_or("")).expect("args are a JSON string");
    assert_eq!(args["q"], "veles");
    assert_eq!(args["limit"], 3);
    // The WHOLE prompt. Anthropic's input_tokens is only what came after
    // the last cache breakpoint (101 here); the 5 written and the 90 read
    // were sent too. The engine reads prompt_tokens the OpenAI way, as
    // everything the model received: it is the compaction bar's ground
    // truth and the denominator of every cache share. Passing input_tokens
    // through as prompt_tokens made a cached 26k prompt report 2 tokens,
    // so compaction never fired and the status bar read "1317350% cached".
    assert_eq!(
        s.prompt,
        Some(196),
        "prompt_tokens = input + cache write + cache read"
    );
    assert_eq!(s.completion, Some(42));
    // Cache accounting, which neither driver computes: it comes from
    // the engine's own `cache_usage`, which already reads Anthropic's
    // spellings. That is the contract paying for itself.
    assert_eq!(s.cached, Some(90), "cache_read_input_tokens must survive");
    assert_eq!(
        s.cache_write,
        Some(5),
        "cache_creation_input_tokens must survive"
    );

    // ── the outbound half ────────────────────────────────────────────
    //
    // POSTs only. The driver also GETs the capability catalog on a turn that
    // did not pin an output cap (pinned by
    // `an_unpinned_turn_discovers_the_output_cap`), and `fixture_server`
    // answers every request with the same fixture — so the capture holds a
    // GET ahead of each POST. Indexing the raw capture would hand
    // `assert_outbound` a catalog request, and the assertions would be about
    // the wrong bytes.
    let requests = seen.lock().unwrap().clone();
    let posts: Vec<&String> = requests.iter().filter(|r| r.starts_with("POST ")).collect();
    assert_eq!(
        posts.len(),
        2,
        "both drivers must have called the endpoint; capture was {requests:?}"
    );
    assert_outbound(posts[0], "tcl");
    assert_outbound(posts[1], "rust");
}

// ── the features the driver grew ────────────────────────────────────────
//
// Everything below runs against BOTH implementations. That is not
// thoroughness for its own sake: `driver_parity`'s whole claim is that the
// two plugin languages are the same capability, and a feature tested against
// one of them is a feature the other is free to get wrong.

/// The drivers that actually registered. Fewer than `BOTH` when the `.so`
/// was not built, which the loader already reported (and failed on, when
/// `VELES_REQUIRE_PLUGIN_SO` is set).
fn available_drivers() -> Vec<&'static str> {
    let got: Vec<&'static str> = BOTH
        .iter()
        .copied()
        .filter(|d| veles_core::providers::driver(d).is_some())
        .collect();
    assert!(
        !got.is_empty(),
        "no anthropic driver registered — both_drivers() did not load one"
    );
    got
}

/// A user message carrying an attachment, the way the engine builds one:
/// `media::attach_user_media` resolves the file to a data URL just before the
/// request goes out, and `build_body` turns that into OpenAI `image_url`
/// parts.
fn request_with_image(model: &str) -> ChatRequest {
    let mut user = ChatMessage::user("what is in this image");
    user.attachments = vec![veles_core::media::Attachment {
        source: "pic.png".into(),
        mime: "image/png".into(),
        data_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==".into(),
    }];
    ChatRequest {
        model: model.to_string(),
        messages: vec![user],
        tools: vec![],
        reasoning_effort: None,
        stream: false,
        replay_reasoning: false,
        cache_mode: CacheMode::Off,
        extra_body: None,
    }
}

/// The outbound body of the single request a fixture captured, parsed.
fn body_of(requests: &[String], nth: usize) -> serde_json::Value {
    let raw = requests
        .get(nth)
        .unwrap_or_else(|| panic!("expected a request at index {nth}, got {}", requests.len()));
    let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("body was not JSON ({e}): {body}"))
}

/// The outbound bodies of the POSTs a fixture captured, in order.
///
/// Filtering matters: a driver that consults its endpoint's capability
/// catalog makes a `GET /v1/models` BEFORE its `POST /v1/messages`, so the
/// capture interleaves the two. Indexing the raw capture would hand the test
/// a catalog request and let it "pass" against the wrong bytes.
fn posts_of(requests: &[String]) -> Vec<serde_json::Value> {
    requests
        .iter()
        .filter(|raw| raw.starts_with("POST "))
        .map(|raw| {
            let body = raw.split("\r\n\r\n").nth(1).unwrap_or("");
            serde_json::from_str(body)
                .unwrap_or_else(|e| panic!("POST body was not JSON ({e}): {body}"))
        })
        .collect()
}

/// An image must reach the endpoint as an `image` block with the media type
/// and payload SPLIT OUT of the data URL.
///
/// This is the bug the old driver had and never noticed: it read `content`
/// with a string getter, so a user message the engine had turned into a
/// parts array became an EMPTY STRING. The image did not fail — it silently
/// vanished, and the model answered a question nobody had finished asking.
/// A silent data loss is worse than a 400, because a 400 gets fixed.
#[tokio::test]
async fn an_attachment_reaches_the_endpoint_as_an_image_block() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    for d in &drivers {
        drive_with(d, &url, default_route(), request_with_image("claude-x"))
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
    }

    let requests = seen.lock().unwrap().clone();
    let posts = posts_of(&requests);
    assert_eq!(posts.len(), drivers.len(), "one POST per driver");
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        let user = &v["messages"][0];
        assert_eq!(user["role"], "user", "{d}");
        let blocks = user["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{d}: content must be a block array, got {:?}", user));
        let image = blocks
            .iter()
            .find(|b| b["type"] == "image")
            .unwrap_or_else(|| panic!("{d}: no image block in {blocks:?}"));
        assert_eq!(image["source"]["type"], "base64", "{d}");
        assert_eq!(image["source"]["media_type"], "image/png", "{d}");
        assert_eq!(
            image["source"]["data"], "iVBORw0KGgoAAAANSUhEUg==",
            "{d}: the payload must survive intact"
        );
        // And the question travels with it — the text half of the parts
        // array is the half that used to be all that survived.
        assert!(
            blocks.iter().any(|b| b["type"] == "text"
                && b["text"].as_str() == Some("what is in this image")),
            "{d}: the user's text must survive beside the image"
        );
    }
}

/// Plain-text message content that happens to start with a literal `[` must
/// reach the endpoint as that text, not be mistaken for a parts array.
///
/// This is not a corner case the test invented: the engine wraps a message
/// that crossed surfaces as "[origin] this message arrived via the irc
/// surface" (gateway.rs), and that wrapper is ordinary `content`. A driver
/// that decides "is this a parts array?" by looking at the FIRST CHARACTER
/// sends it down the array path, where the next `json_get` on
/// "[origin] …" cannot parse and RAISES, killing the turn with "driver
/// failed: json_get: bad json: expected value at line 1 column 2" — column 2
/// being the character right after the `[` that was read as a bracket. Found
/// live 2026-10-16 configuring a real Anthropic route; the fix is that
/// `is_json_array` now parses before it looks (plugin.tcl).
///
/// Both drivers agree because the Rust one always matched on the parsed
/// `Value::Array` and never had the bug; this pins the Tcl one to it.
#[tokio::test]
async fn plain_text_starting_with_a_bracket_reaches_the_endpoint_intact() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    // The engine's own cross-surface wrapper: ordinary text, leading `[`.
    let wrapped = "[origin] this message arrived via the irc surface";
    let req = ChatRequest {
        model: "claude-x".into(),
        messages: vec![ChatMessage::user(wrapped)],
        tools: vec![],
        reasoning_effort: None,
        stream: false,
        replay_reasoning: false,
        cache_mode: CacheMode::Off,
        extra_body: None,
    };

    for d in &drivers {
        // The turn must SUCCEED — the old code raised here and never posted.
        drive_with(d, &url, default_route(), req.clone())
            .await
            .unwrap_or_else(|e| panic!("{d}: a leading-bracket text content must not fail: {e}"));
    }

    let requests = seen.lock().unwrap().clone();
    let posts = posts_of(&requests);
    assert_eq!(posts.len(), drivers.len(), "one POST per driver");
    for (i, d) in drivers.iter().enumerate() {
        let user = &posts[i]["messages"][0];
        assert_eq!(user["role"], "user", "{d}");
        // The wire shape for a plain-text user message is a single `text`
        // block (both drivers do this, and the parity test above pins that
        // they agree). The point here is the TEXT survived verbatim — it was
        // not truncated at the `[`, dropped as an unparseable "parts array",
        // or split on the bracket.
        let blocks = user["content"]
            .as_array()
            .unwrap_or_else(|| panic!("{d}: content must be a block array, got {:?}", user));
        assert_eq!(blocks.len(), 1, "{d}: one text block, got {blocks:?}");
        assert_eq!(blocks[0]["type"], "text", "{d}");
        assert_eq!(
            blocks[0]["text"].as_str(),
            Some(wrapped),
            "{d}: the text must reach the wire unchanged"
        );
    }
}

/// A tool loop as the engine sends it: a question, two tool rounds.
/// The question and the first result are text that IS valid JSON array
/// syntax — which a tool's output very often is.
fn request_with_tool_rounds(mode: CacheMode) -> ChatRequest {
    let call = |id: &str, text: &str| {
        let mut m = ChatMessage::assistant(text, None);
        m.tool_calls = Some(serde_json::json!([{
            "id": id, "type": "function",
            "function": { "name": "search", "arguments": "{}" }
        }]));
        m
    };
    let mut req = request("claude-x");
    req.cache_mode = mode;
    req.messages = vec![
        req.messages[0].clone(),
        ChatMessage::user(r#"["a", "b"]"#),
        call("toolu_1", "Let me look."),
        ChatMessage::tool_result("toolu_1", "search", "[1, 2, 3]".into()),
        call("toolu_2", ""),
        ChatMessage::tool_result("toolu_2", "search", "an agent".into()),
    ];
    req
}

fn cache_controls(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(m) => m
            .iter()
            .map(|(k, x)| usize::from(k == "cache_control") + cache_controls(x))
            .sum(),
        serde_json::Value::Array(a) => a.iter().map(cache_controls).sum(),
        _ => 0,
    }
}

/// The tool_result block for `id`, wherever the driver put it.
fn tool_result_block(v: &serde_json::Value, id: &str) -> serde_json::Value {
    v["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .find(|b| b["type"] == "tool_result" && b["tool_use_id"] == id)
        .unwrap_or_else(|| panic!("no tool_result for {id} in {v}"))
}

/// Explicit mode marks the end of every request — mid-turn, a TOOL
/// result — and the engine marks it the only way the OpenAI shape can:
/// the content becomes a one-part array. Both drivers read tool content
/// as a plain string, so the marker never reached the wire and the
/// session's rolling cache died at the first mid-turn user message
/// (2026-10-28, sessions bf03ad and cb3de7: reads fell to the 23.5k
/// system+tools prefix and stayed there). Worse, the Tcl driver handed
/// the model the parts array AS TEXT — `[{"cache_control":…,"text":…}]`
/// — and the Rust one handed it an empty result.
///
/// The marker must land on the tool_result block (Anthropic caches at
/// any content block) with the text intact, and text that merely LOOKS
/// like an array must stay text, marked or not.
#[tokio::test]
async fn a_breakpoint_on_a_tool_result_reaches_the_wire_with_its_text() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    for mode in [CacheMode::Explicit, CacheMode::Off] {
        for d in &drivers {
            drive_with(d, &url, default_route(), request_with_tool_rounds(mode))
                .await
                .unwrap_or_else(|e| panic!("{d} {mode:?}: {e}"));
        }
    }

    let posts = posts_of(&seen.lock().unwrap().clone());
    assert_eq!(
        posts.len(),
        2 * drivers.len(),
        "one POST per driver per mode"
    );
    for (n, v) in posts.iter().enumerate() {
        let d = drivers[n % drivers.len()];
        let explicit = n < drivers.len();

        // The question survives as the text it was, not as a "parts
        // array" with no parts the driver recognised.
        let question = &v["messages"][0];
        assert_eq!(question["role"], "user", "{d}: {v}");
        assert_eq!(
            question["content"][0]["text"].as_str(),
            Some(r#"["a", "b"]"#),
            "{d}: a user message that parses as JSON must still be its text: {v}"
        );

        let first = tool_result_block(v, "toolu_1");
        let second = tool_result_block(v, "toolu_2");
        assert_eq!(first["content"], "[1, 2, 3]", "{d}: tool text intact: {v}");
        assert_eq!(second["content"], "an agent", "{d}: tool text intact: {v}");

        if explicit {
            // system + tools + the previous request's tail (the first
            // result) + this request's tail (the second).
            assert_eq!(cache_controls(v), 4, "{d}: {v}");
            assert_eq!(
                v["system"][0]["cache_control"]["type"], "ephemeral",
                "{d}: {v}"
            );
            assert_eq!(
                v["tools"][0]["cache_control"]["type"], "ephemeral",
                "{d}: {v}"
            );
            assert_eq!(first["cache_control"]["type"], "ephemeral", "{d}: {v}");
            assert_eq!(second["cache_control"]["type"], "ephemeral", "{d}: {v}");
        } else {
            assert_eq!(cache_controls(v), 0, "{d}: off means nothing: {v}");
        }
    }
}

/// A capability catalog: what `/v1/models` says about one model.
///
/// The shapes are the live API's, trimmed to the three fields the driver
/// reads. `modern` is every 4.6+ model; `legacy` is the dated 4.5 trio.
const CATALOG_MODERN: &str = r#"{"data":[{"id":"claude-modern","max_tokens":128000,"capabilities":{"effort":{"supported":true},"thinking":{"types":{"adaptive":{"supported":true},"enabled":{"supported":false}}}}}]}"#;
const CATALOG_LEGACY: &str = r#"{"data":[{"id":"claude-legacy","max_tokens":64000,"capabilities":{"effort":{"supported":false},"thinking":{"types":{"adaptive":{"supported":false},"enabled":{"supported":true}}}}}]}"#;

fn request_with_effort(model: &str, effort: &str) -> ChatRequest {
    ChatRequest {
        reasoning_effort: Some(effort.to_string()),
        ..request(model)
    }
}

/// The driver asks its endpoint what the model supports rather than carrying
/// a table of model families — and the two shapes it can be asked for are
/// mutually exclusive, so guessing is a hard 400.
///
/// Measured against the live API 2026-10-16: `output_config.effort` is
/// rejected by `claude-sonnet-4-5` ("This model does not support the effort
/// parameter") while `thinking:{type:enabled}` is rejected by every 5-series
/// model ("Use thinking.type.adaptive"). `claude-opus-4-5` accepts effort and
/// rejects adaptive, and its sibling `claude-sonnet-4-5` rejects both — so no
/// generation rule separates them. Only asking works.
#[tokio::test]
async fn thinking_and_effort_follow_what_the_endpoint_says() {
    both_drivers().await;
    let drivers = available_drivers();

    // A modern model: adaptive thinking, and effort in output_config.
    let (url, seen) = route_server(CATALOG_MODERN, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            default_route(),
            request_with_effort("claude-modern", "high"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (modern): {e}"));
    }
    let requests = seen.lock().unwrap().clone();
    let posts = posts_of(&requests);
    assert_eq!(
        posts.len(),
        drivers.len(),
        "one POST per driver; the capture also holds the catalog GETs"
    );
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert_eq!(v["thinking"]["type"], "adaptive", "{d}");
        assert_eq!(v["output_config"]["effort"], "high", "{d}");
        // The catalog's own output cap, not a hardcoded number.
        assert_eq!(v["max_tokens"], 128000, "{d}");
    }

    // A legacy model: the budget shape, and NO output_config (it is a 400).
    let (url, seen) = route_server(CATALOG_LEGACY, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            default_route(),
            request_with_effort("claude-legacy", "high"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (legacy): {e}"));
    }
    let requests = seen.lock().unwrap().clone();
    let posts = posts_of(&requests);
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert_eq!(v["thinking"]["type"], "enabled", "{d}");
        assert!(
            v["thinking"]["budget_tokens"]
                .as_u64()
                .is_some_and(|b| b >= 1024),
            "{d}: the endpoint requires budget_tokens >= 1024, got {:?}",
            v["thinking"]
        );
        assert!(
            v["thinking"]["budget_tokens"].as_u64().unwrap_or(u64::MAX)
                < v["max_tokens"].as_u64().unwrap_or(0),
            "{d}: the endpoint requires max_tokens > budget_tokens, got {:?}",
            v
        );
        assert!(
            v.get("output_config").is_none(),
            "{d}: output_config is a 400 on a model that does not support effort, got {v}"
        );
    }
}

/// A model the catalog does not list gets NO capability-dependent fields.
///
/// This is the posture that makes the driver safe against a gateway that
/// will not say what it supports: sending a field a strict endpoint does not
/// know is a hard 400 ("Extra inputs are not permitted", measured live), so
/// the conservative answer is to send nothing inferred. A turn that cannot
/// think beats a turn that cannot run.
#[tokio::test]
async fn an_unknown_model_gets_no_guesses() {
    both_drivers().await;
    let drivers = available_drivers();
    // The catalog answers, but does not list the model asked for. Two calls
    // per driver: the GET that discovers "not listed", then the POST.
    let (url, seen) = route_server(CATALOG_MODERN, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            default_route(),
            request_with_effort("claude-never-heard-of-it", "high"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d}: {e}"));
    }
    let requests = seen.lock().unwrap().clone();
    let posts = posts_of(&requests);
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert!(
            v.get("thinking").is_none(),
            "{d}: no thinking on an unknown model, got {v}"
        );
        assert!(
            v.get("output_config").is_none(),
            "{d}: no output_config on an unknown model, got {v}"
        );
    }
}

/// The sampling gate is TWO questions, and the version's answer wins.
///
/// The catalog says a model takes `temperature` — and under `2023-06-01`
/// it does (measured live 2026-10-16, and again 2026-10-22: haiku-4-5 +
/// temperature is 200). But version `2026-10-21` REMOVED the sampling
/// parameters per-version: the same request under the new wire is a hard
/// 400 on EVERY model, including one whose capability entry still says
/// `thinking.types.enabled.supported = true`. A gate that consulted only
/// the catalog would forward the operator's temperature straight into
/// that 400 — and so would `unknown_caps`'s permissive `sampling: true`,
/// which is the other half of this test.
#[tokio::test]
async fn sampling_follows_the_version_even_when_the_catalog_says_yes() {
    both_drivers().await;
    let drivers = available_drivers();

    let with_temperature = |model: &str| ChatRequest {
        extra_body: Some(serde_json::json!({ "temperature": 0.5, "top_k": 7 })),
        ..request(model)
    };
    let route_at = |version: &str| PluginRoute {
        api_version: version.to_string(),
        ..Default::default()
    };

    // The legacy catalog: `enabled.supported = true`, the predictor that
    // says this model takes sampling — true under the OLD version.
    let (url, seen) = route_server(CATALOG_LEGACY, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            route_at("2023-06-01"),
            with_temperature("claude-legacy"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (2023-06-01): {e}"));
    }
    let posts = posts_of(&seen.lock().unwrap().clone());
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert_eq!(
            v["temperature"], 0.5,
            "{d}: under 2023-06-01 a sampling-capable model must get the \
             operator's temperature, got {v}"
        );
        // The wire carries numbers as floats (the driver reads them with
        // `as_f64` and Tcl's `jnum`): 7 arrives as 7.0, which is the same
        // JSON number and what the endpoint accepts.
        assert_eq!(v["top_k"], 7.0, "{d}: and top_k rides with it");
    }

    // The SAME model, the SAME catalog, the new version: the parameters
    // must be dropped even though the model check says yes.
    let (url, seen) = route_server(CATALOG_LEGACY, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            route_at("2026-10-21"),
            with_temperature("claude-legacy"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (2026-10-21, legacy): {e}"));
    }
    let posts = posts_of(&seen.lock().unwrap().clone());
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert!(
            v.get("temperature").is_none() && v.get("top_k").is_none(),
            "{d}: 2026-10-21 removed the sampling parameters per-VERSION — \
             sending them is a hard 400 even on a model the catalog calls \
             sampling-capable, got {v}"
        );
    }

    // Unknown caps: `unknown_caps()` is permissively `sampling: true`
    // (an operator's own extra_body is their request, not our guess) —
    // but only on a version that still accepts it. Under the new one the
    // permissive default must NOT forward the parameters either.
    let (url, seen) = route_server(CATALOG_MODERN, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            route_at("2026-10-21"),
            with_temperature("claude-never-heard-of-it"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (2026-10-21, unknown): {e}"));
    }
    let posts = posts_of(&seen.lock().unwrap().clone());
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert!(
            v.get("temperature").is_none() && v.get("top_k").is_none(),
            "{d}: the version gate must override unknown_caps' permissive \
             sampling default, got {v}"
        );
    }

    // A version string that is not an Anthropic date (a gateway's own
    // label) does NOT trigger the ban: the rule is Anthropic's, and
    // dropping a parameter a proxy would have accepted is the worse
    // guess. Unknown caps, non-date version → forwarded.
    let (url, seen) = route_server(CATALOG_MODERN, ANTHROPIC_RESPONSE, false, 200).await;
    for d in &drivers {
        drive_with(
            d,
            &url,
            route_at("gateway-v2"),
            with_temperature("claude-never-heard-of-it"),
        )
        .await
        .unwrap_or_else(|e| panic!("{d} (non-date version): {e}"));
    }
    let posts = posts_of(&seen.lock().unwrap().clone());
    for (i, d) in drivers.iter().enumerate() {
        let v = &posts[i];
        assert_eq!(
            v["temperature"], 0.5,
            "{d}: a non-date version string must not trigger Anthropic's \
             per-version ban, got {v}"
        );
    }
}

/// The endpoint knobs reach the wire. These four exist because
/// "anthropic-compatible" is not one shape: a proxy may mount the wire at a
/// different path, authenticate with a bearer token instead of `x-api-key`,
/// require a different wire version, and cap output differently. A driver
/// that hardcoded any of them would need a fork per gateway.
#[tokio::test]
async fn the_endpoint_knobs_reach_the_wire() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    let knobs = || veles_core::providers::PluginRoute {
        endpoint_path: "/gateway/v1/messages".into(),
        auth_style: "bearer".into(),
        api_version: "2025-01-01".into(),
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    for d in &drivers {
        drive_with(d, &url, knobs(), request("claude-x"))
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
    }

    let requests = seen.lock().unwrap().clone();
    for (i, d) in drivers.iter().enumerate() {
        let raw = &requests[i];
        assert!(
            raw.starts_with("POST /gateway/v1/messages "),
            "{d}: endpoint_path must decide where the request goes, got {}",
            raw.lines().next().unwrap_or("")
        );
        let lower = raw.to_lowercase();
        assert!(
            lower.contains("authorization: bearer sk-fixture"),
            "{d}: auth_style = \"bearer\" must send a bearer token:\n{raw}"
        );
        assert!(
            !lower.contains("x-api-key"),
            "{d}: and must NOT also send x-api-key — one credential, one header:\n{raw}"
        );
        assert!(
            lower.contains("anthropic-version: 2025-01-01"),
            "{d}: api_version must override the driver's pin:\n{raw}"
        );
        assert_eq!(
            body_of(&requests, i)["max_tokens"],
            4096,
            "{d}: max_output_tokens must decide the output cap"
        );
    }
}

/// A configured `max_output_tokens` is honoured, and consulting the catalog
/// is SKIPPED — a pinned cap needs no discovery, so the turn is exactly one
/// HTTP call.
///
/// The `requests.len()` assertion is the part that carries weight: it is what
/// proves the cap came from config rather than from a catalog read that
/// happened to return the same number.
#[tokio::test]
async fn a_configured_output_cap_does_not_need_the_catalog() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, seen) = fixture_server(ANTHROPIC_RESPONSE).await;

    let knobs = || veles_core::providers::PluginRoute {
        max_output_tokens: Some(2048),
        ..Default::default()
    };
    for d in &drivers {
        // `request()` has reasoning_effort: None and the cap is pinned, so
        // neither decision needs the catalog.
        drive_with(d, &url, knobs(), request("claude-x"))
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
    }

    let requests = seen.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        drivers.len(),
        "one call per driver, and no catalog read when the cap is pinned — got {}",
        requests.len()
    );
    for (i, d) in drivers.iter().enumerate() {
        assert_eq!(body_of(&requests, i)["max_tokens"], 2048, "{d}");
    }
}

/// A turn that did NOT pin an output cap asks the catalog what the model's
/// cap is, and sends that.
///
/// Anthropic REQUIRES `max_tokens`, and the engine never sets one, so the
/// driver has to invent a number. Inventing a constant is how a long answer
/// gets silently truncated on a 128000-token model: the turn does not fail,
/// it just stops early, which reads to the user as "the model cut itself off
/// mid-sentence". The catalog answers it per model, so the number is right
/// for a model the driver has never heard of — including `fable-5`, which is
/// in no table anybody could have written down.
#[tokio::test]
async fn an_unpinned_turn_discovers_the_output_cap() {
    both_drivers().await;
    let drivers = available_drivers();
    // CATALOG_MODERN says claude-modern's cap is 128000.
    let (url, seen) = route_server(CATALOG_MODERN, ANTHROPIC_RESPONSE, false, 200).await;

    for d in &drivers {
        // No reasoning_effort AND no pinned cap: the discovery decision is
        // driven entirely by needing a max_tokens.
        drive_with(d, &url, default_route(), request("claude-modern"))
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));
    }

    let requests = seen.lock().unwrap().clone();
    assert!(
        requests.iter().any(|r| r.starts_with("GET /v1/models")),
        "the catalog must be consulted to find the cap; capture was {:?}",
        requests
            .iter()
            .map(|r| r.lines().next().unwrap_or(""))
            .collect::<Vec<_>>()
    );
    let posts = posts_of(&requests);
    assert_eq!(posts.len(), drivers.len(), "one POST per driver");
    for (i, d) in drivers.iter().enumerate() {
        assert_eq!(
            posts[i]["max_tokens"], 128000,
            "{d}: the discovered cap, not the driver's fallback"
        );
    }
}

/// A real 429 from the endpoint reaches the engine as a rate limit, which is
/// what makes the route fail over.
///
/// The engine's `should_fallback` fires on 429, 5xx, network and truncation
/// and nothing else. Before the status was classified, every driver error was
/// a `Client` error, so an Anthropic-compatible gateway that rate-limited
/// ended the turn instead of moving to the next route in the chain. The core
/// half of this is pinned in `providers.rs`'s
/// `a_driver_reported_status_decides_whether_the_route_fails_over`; this is
/// the driver half — that it reports the endpoint's status at all, and
/// surfaces the endpoint's own message rather than a transport wrapper.
#[tokio::test]
async fn a_rate_limited_endpoint_fails_over_instead_of_failing_the_turn() {
    both_drivers().await;
    let drivers = available_drivers();
    let envelope =
        r#"{"type":"error","error":{"type":"rate_limit_error","message":"too many tokens"}}"#;
    let (url, _seen) = route_server(CATALOG_MODERN, envelope, false, 429).await;

    for d in &drivers {
        let err = drive_with(d, &url, default_route(), request("claude-modern"))
            .await
            .expect_err("a 429 is an error, not a completion");
        assert!(
            matches!(err, veles_core::providers::ProviderError::RateLimited(_)),
            "{d}: a 429 must be RateLimited so the chain moves on, got {err:?}"
        );
        assert!(
            err.should_fallback(),
            "{d}: and it must actually fail over: {err}"
        );
        // The endpoint's own words, not "http 429: {\"type\":\"error\"…}".
        let msg = err.to_string();
        assert!(msg.contains("rate_limit_error"), "{d}: {msg}");
        assert!(msg.contains("too many tokens"), "{d}: {msg}");
    }
}

/// A 5xx is a server error, and server errors fail over too.
#[tokio::test]
async fn a_server_error_fails_over() {
    both_drivers().await;
    let drivers = available_drivers();
    let envelope = r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#;
    let (url, _seen) = route_server(CATALOG_MODERN, envelope, false, 529).await;

    for d in &drivers {
        let err = drive_with(d, &url, default_route(), request("claude-modern"))
            .await
            .expect_err("a 529 is an error");
        assert!(
            matches!(
                err,
                veles_core::providers::ProviderError::Server { status: 529, .. }
            ),
            "{d}: a 5xx must be Server, got {err:?}"
        );
        assert!(err.should_fallback(), "{d}: a 5xx fails over: {err}");
    }
}

/// One SSE stream, as the live API frames it: `message_start`, then typed
/// content blocks, then `message_delta` with the authoritative usage, then
/// `message_stop` — with a `ping` in the middle, which a driver that failed on
/// an event it did not recognize would trip over.
///
/// The tool arguments are split across TWO `input_json_delta` events on
/// purpose. They are JSON fragments that only parse once concatenated, and
/// assembling them is the part of streaming that a naive implementation gets
/// wrong by parsing each fragment as it arrives.
const SSE_STREAM: &str = r#"event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":101,"output_tokens":1,"cache_creation_input_tokens":5,"cache_read_input_tokens":90}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"weighing "}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"it up"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"EqQBCgI"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: ping
data: {"type": "ping"}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Here is "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"the answer."}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_9","name":"search"}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"q\":\"veles\","}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"limit\":3}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":2}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42,"output_tokens_details":{"thinking_tokens":7}}}

event: message_stop
data: {"type":"message_stop"}

"#;

/// THE streaming assertion: the same wire answer, delivered as events instead
/// of as one body, must produce the SAME `Completion`.
///
/// Both drivers are deliberately written to reassemble the stream into the
/// non-streaming shape and hand that to the one response translator, so the
/// two paths cannot disagree about what a completion is. This test is what
/// makes that a property rather than an intention — and it compares against
/// the fixture the non-streaming test already asserts is the RIGHT answer,
/// so "same as streaming" cannot be satisfied by both being wrong together.
#[tokio::test]
async fn a_streamed_answer_matches_the_same_answer_unstreamed() {
    both_drivers().await;
    let drivers = available_drivers();

    // The unstreamed baseline, from the JSON fixture.
    let (url, _seen) = fixture_server(ANTHROPIC_RESPONSE).await;
    let mut baseline = None;
    for d in &drivers {
        let c = drive_with(d, &url, default_route(), request("claude-x"))
            .await
            .unwrap_or_else(|e| panic!("{d} (unstreamed): {e}"));
        baseline.get_or_insert(shape(&c));
        assert_eq!(
            baseline.as_ref().unwrap(),
            &shape(&c),
            "{d}: both drivers must agree unstreamed before streaming means anything"
        );
    }
    let baseline = baseline.expect("at least one driver");

    // Now the same answer over SSE.
    let (url, _seen) = route_server(CATALOG_MODERN, SSE_STREAM, true, 200).await;
    for d in &drivers {
        let mut streamed = request("claude-x");
        streamed.stream = true;
        let c = drive_with(d, &url, default_route(), streamed)
            .await
            .unwrap_or_else(|e| panic!("{d} (streamed): {e}"));
        let s = shape(&c);
        assert_eq!(
            s, baseline,
            "{d}: streaming must not change the answer — content, reasoning, \
             tool call and all four token counters"
        );
        // The reasoning tokens specifically: Anthropic reports them under
        // output_tokens_details.thinking_tokens and the engine's
        // `reasoning_usage` reads only the OpenAI spelling, so the driver has
        // to translate or the number is silently lost.
        assert_eq!(
            c.reasoning_tokens,
            Some(7),
            "{d}: thinking tokens must survive the stream, not be billed as answer output"
        );
    }
}

/// Text arrives as deltas WHILE the stream runs, which is the entire point of
/// streaming. A driver that buffered and emitted once at the end would pass
/// every assertion above and still feel like the old behaviour.
#[tokio::test]
async fn streaming_emits_deltas_as_they_arrive() {
    both_drivers().await;
    let drivers = available_drivers();
    let (url, _seen) = route_server(CATALOG_MODERN, SSE_STREAM, true, 200).await;

    for d in drivers {
        let mut req = request("claude-x");
        req.stream = true;
        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let route = veles_core::providers::PluginRoute {
            driver: d.to_string(),
            base_url: url.clone(),
            api_key: "sk-fixture".into(),
            ..Default::default()
        };
        veles_core::providers::RouteClient::Plugin(route)
            .chat_stream(&req, "anthropic-under-test", None, move |ev| {
                use veles_core::providers::StreamEvent;
                let mut v = sink.lock().unwrap();
                match ev {
                    StreamEvent::Delta(t) => v.push(format!("text:{t}")),
                    StreamEvent::ReasoningDelta(t) => v.push(format!("think:{t}")),
                    _ => {}
                }
            })
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"));

        let got = events.lock().unwrap().clone();
        // Two text deltas and two thinking deltas, in that order — the
        // thinking block precedes the text block in the stream.
        assert_eq!(
            got,
            vec![
                "think:weighing ".to_string(),
                "think:it up".to_string(),
                "text:Here is ".to_string(),
                "text:the answer.".to_string(),
            ],
            "{d}: the deltas must arrive split, in stream order — a single \
             buffered emit would be [\"text:Here is the answer.\"]"
        );
    }
}
