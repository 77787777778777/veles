//! The ABI 4 example plugin: every surface a native plugin has, on the
//! safe SDK (`veles-plugin-abi-sdk`), exercised by
//! `crates/veles-core/tests/plugin_so.rs` and the conformance suite.
//!
//! It registers, at init:
//!   - a `post_llm_call` hook (a verdict on "so-magic", no opinion
//!     otherwise);
//!   - a sync tool `so_echo`, a tool `so_ask` that CALLS THE ENGINE
//!     through `host.call`, an ASYNC tool `so_wait` whose future is
//!     woken by a host timer, and `so_panic`, whose panic the SDK turns
//!     into an error string;
//!   - the slash command `soask`;
//!   - a service, `conformance.point`: the conformance suite's override
//!     point, wrapped through `service_next`;
//!   - a gateway filter (`!soping` → a rewrite);
//!   - the platform `soecho`: what the gateway sends it is kept for
//!     `so_delivered` to read, and `so_inbound` publishes a message on
//!     it — nothing echoes by itself, so a test stays in control;
//!   - management tools for the host services: `so_timer` /
//!     `so_timer_cancel` / `so_timer_log`, `so_tcp` / `so_tcp_send` /
//!     `so_tcp_close` / `so_tcp_log`, `so_late_hook` (a hook registered
//!     from a callback, attributed by the plugin id) and
//!     `so_unregister`.
//!
//! Panics are allowed here on purpose: the SDK catches them, and the
//! test proves it. A plugin on the RAW ABI must not panic.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use veles_plugin_abi_sdk::{veles_plugin, Filter, Handle, Host, SocketEvent, ToolFuture};

/// The host, as of the LATEST init: the integration tests load this
/// one file under several names in one process, and a `.so` mapped
/// once shares its statics, so a `OnceLock` would pin every later copy
/// to the first plugin's id. A deployed plugin is initialised once.
static HOST: Mutex<Option<Host>> = Mutex::new(None);

/// What the gateway delivered to the `soecho` platform, newest last.
static DELIVERED: Mutex<Vec<String>> = Mutex::new(Vec::new());
/// Timer firings, and the last timer's handle.
static TIMER_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LAST_TIMER: Mutex<Handle> = Mutex::new(0);
/// Socket events, and the last socket's handle.
static TCP_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static LAST_TCP: Mutex<Handle> = Mutex::new(0);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn host() -> Host {
    lock(&HOST).expect("initialised")
}

/// The tiniest JSON reader a sample needs: a top-level string or
/// number field.
fn field<'a>(doc: &'a str, key: &str) -> Option<&'a str> {
    let pat = format!("\"{key}\":");
    let rest = &doc[doc.find(&pat)? + pat.len()..];
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"')?;
        Some(&stripped[..end])
    } else {
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == '.'))
            .unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

fn num(doc: &str, key: &str, default: u64) -> u64 {
    field(doc, key)
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `so_wait`'s future: ready once the timer the tool started has put
/// the answer in, woken through the waker it parked.
struct WaitState {
    result: Option<String>,
    waker: Option<Waker>,
}

struct Wait(Arc<Mutex<WaitState>>);

impl Future for Wait {
    type Output = Result<String, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut st = lock(&self.0);
        if let Some(r) = st.result.take() {
            return Poll::Ready(Ok(r));
        }
        st.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

fn init(h: &Host) -> Result<(), String> {
    *lock(&HOST) = Some(*h);

    h.register_hook("post_llm_call", |args| {
        if args.contains("so-magic") {
            Some(r#"{"decision":"saw-it-from-so"}"#.to_string())
        } else {
            None
        }
    });

    // An override of the conformance suite's point (PLAN-extension-api.md
    // D2): what the layer below answers, with "so" after it. The input
    // is {"params", "caller", "session_id", "next"}.
    h.register_service(
        "conformance.point",
        "the conformance suite's point, wrapped",
        "",
        |input| {
            let host = (*lock(&HOST)).ok_or("no host yet")?;
            let next = veles_plugin_abi_sdk::json::field(input, "next")
                .ok_or_else(|| format!("no next token in {input}"))?;
            let below = host.call("service_next", &[&next, "{}"])?;
            // `{"via":[…]}`: the same list with "so" appended.
            let at = below
                .rfind(']')
                .ok_or_else(|| format!("no list in {below}"))?;
            Ok(format!("{},\"so\"{}", &below[..at], &below[at..]))
        },
    );

    h.register_tool(
        "so_echo",
        "Echo the args back with a marker. Proves a .so tool call works.",
        r#"{"type":"object","properties":{"text":{"type":"string"}}}"#,
        |args| Ok(format!("so_echo: {args}")),
    );

    h.register_tool(
        "so_ask",
        "Ask the ENGINE something through host_call. Proves the ABI is two-way.",
        r#"{"type":"object","properties":{}}"#,
        |_args| {
            let sessions = host().call("sessions", &[])?;
            let first = host().call("json_get", &[&sessions, "0", "id"])?;
            Ok(format!("so_ask: first_session={first}"))
        },
    );

    h.register_tool_async(
        "so_wait",
        "Answer after `ms` milliseconds without parking a thread: a future woken by a host timer (`panic` hands out one that panics when polled).",
        r#"{"type":"object","properties":{"ms":{"type":"integer"},"panic":{"type":"boolean"}}}"#,
        |args| -> ToolFuture {
            if args.contains("\"panic\":true") {
                return Box::pin(std::future::poll_fn(
                    |_cx| -> Poll<Result<String, String>> { panic!("boom from the .so's future") },
                ));
            }
            let ms = num(args, "ms", 50);
            let state = Arc::new(Mutex::new(WaitState {
                result: None,
                waker: None,
            }));
            let st2 = Arc::clone(&state);
            host().timer_after(ms, move || {
                let mut st = lock(&st2);
                st.result = Some(format!("so_wait: woke after {ms} ms"));
                if let Some(w) = st.waker.take() {
                    w.wake();
                }
            });
            Box::pin(Wait(state))
        },
    );

    h.register_tool(
        "so_panic",
        "Panics. The SDK catches it; the model sees an error string.",
        r#"{"type":"object","properties":{}}"#,
        |_args| -> Result<String, String> { panic!("boom from the .so") },
    );

    h.register_command(
        "soask",
        "A slash command implemented in an out-of-tree .so.",
        |args| format!("The .so command was asked about: {args}"),
    );

    h.register_gateway_filter(|_platform, _chat, text| {
        if text.trim_start() == "!soping" {
            Filter::Rewrite("pong from the .so".to_string())
        } else {
            Filter::None
        }
    });

    h.register_platform("soecho", false, |outbound_json| {
        lock(&DELIVERED).push(outbound_json.to_string());
        Ok(())
    });
    h.register_tool(
        "so_delivered",
        "The outbound messages the gateway handed the soecho platform, newest last, one per line.",
        r#"{"type":"object","properties":{}}"#,
        |_args| Ok(lock(&DELIVERED).join("\n")),
    );
    h.register_tool(
        "so_inbound",
        "Publish an inbound message on the soecho platform.",
        r#"{"type":"object","properties":{"text":{"type":"string"},"chat_id":{"type":"string"}}}"#,
        |args| {
            let text = field(args, "text").unwrap_or("hello");
            let chat = field(args, "chat_id").unwrap_or("room");
            let json = format!(
                "{{\"platform\":\"soecho\",\"chat_id\":\"{chat}\",\"sender\":\"so\",\"text\":\"{text}\"}}"
            );
            host()
                .platform_inbound(&json)
                .map(|_| "so_inbound: published".to_string())
                .map_err(|e| format!("{e:?}"))
        },
    );

    h.register_tool(
        "so_timer",
        "Start a host timer (`ms`); it appends `fired` to the timer log.",
        r#"{"type":"object","properties":{"ms":{"type":"integer"}}}"#,
        |args| {
            let ms = num(args, "ms", 20);
            let h = host().timer_after(ms, move || {
                lock(&TIMER_LOG).push(format!("fired after {ms} ms"));
            });
            *lock(&LAST_TIMER) = h;
            Ok(format!("so_timer: handle {h}"))
        },
    );
    h.register_tool(
        "so_timer_cancel",
        "Cancel the last timer.",
        r#"{"type":"object","properties":{}}"#,
        |_args| {
            let h = *lock(&LAST_TIMER);
            match host().timer_cancel(h) {
                Ok(()) => Ok(format!("so_timer_cancel: cancelled {h}")),
                Err(e) => Ok(format!("so_timer_cancel: {e:?}")),
            }
        },
    );
    h.register_tool(
        "so_timer_log",
        "The timer log, one line per firing.",
        r#"{"type":"object","properties":{}}"#,
        |_args| Ok(lock(&TIMER_LOG).join("\n")),
    );

    h.register_tool(
        "so_tcp",
        "Connect to host:port (tls optional); events go to the tcp log; `send` is written once connected.",
        r#"{"type":"object","properties":{"host":{"type":"string"},"port":{"type":"integer"},"tls":{"type":"boolean"},"send":{"type":"string"}}}"#,
        |args| {
            let hostname = field(args, "host").unwrap_or("127.0.0.1").to_string();
            let port = num(args, "port", 0) as u16;
            let tls = args.contains("\"tls\":true");
            let payload = field(args, "send").map(str::to_string);
            let h = host().tcp_connect(&hostname, port, tls, move |ev| {
                let line = match &ev {
                    SocketEvent::Connected => "connected".to_string(),
                    SocketEvent::Data(d) => format!("data: {}", String::from_utf8_lossy(d)),
                    SocketEvent::Closed => "closed".to_string(),
                    SocketEvent::Error(e) => format!("error: {e}"),
                };
                lock(&TCP_LOG).push(line);
                if matches!(ev, SocketEvent::Connected) {
                    if let Some(p) = &payload {
                        let h = *lock(&LAST_TCP);
                        let _ = host().tcp_send(h, p.as_bytes());
                    }
                }
            });
            *lock(&LAST_TCP) = h;
            Ok(format!("so_tcp: handle {h}"))
        },
    );
    h.register_tool(
        "so_tcp_send",
        "Write `text` on the last socket.",
        r#"{"type":"object","properties":{"text":{"type":"string"}}}"#,
        |args| {
            let text = field(args, "text").unwrap_or("");
            let h = *lock(&LAST_TCP);
            host()
                .tcp_send(h, text.as_bytes())
                .map(|_| "so_tcp_send: queued".to_string())
                .map_err(|e| format!("{e:?}"))
        },
    );
    h.register_tool(
        "so_tcp_close",
        "Close the last socket.",
        r#"{"type":"object","properties":{}}"#,
        |_args| {
            let h = *lock(&LAST_TCP);
            host()
                .tcp_close(h)
                .map(|_| "so_tcp_close: closing".to_string())
                .map_err(|e| format!("{e:?}"))
        },
    );
    h.register_tool(
        "so_tcp_log",
        "The socket event log, one line per event.",
        r#"{"type":"object","properties":{}}"#,
        |_args| Ok(lock(&TCP_LOG).join("\n")),
    );

    h.register_tool(
        "so_late_hook",
        "Register a `pre_tool_call` hook from this callback (a LATE registration, attributed by the plugin id).",
        r#"{"type":"object","properties":{}}"#,
        |_args| {
            let h = host().register_hook("pre_tool_call", |args| {
                if args.contains("so-late-magic") {
                    Some(r#"{"decision":"late-hook-saw-it"}"#.to_string())
                } else {
                    None
                }
            });
            Ok(format!("so_late_hook: handle {h}"))
        },
    );
    h.register_tool(
        "so_unregister",
        "Drop a handle this plugin owns.",
        r#"{"type":"object","properties":{"handle":{"type":"integer"}}}"#,
        |args| {
            let h = num(args, "handle", 0);
            match host().unregister(h) {
                Ok(()) => Ok(format!("so_unregister: dropped {h}")),
                Err(e) => Ok(format!("so_unregister: {e:?}")),
            }
        },
    );

    Ok(())
}

veles_plugin!(init);
