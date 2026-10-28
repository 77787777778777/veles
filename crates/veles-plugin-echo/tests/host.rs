// A test/example driver: it spawns onto a scratch runtime it owns;
// the M3 hop (clippy.toml) is for production core/frontends code.
#![allow(clippy::disallowed_methods)]

//! The plugin host (veles_core::pluginhost) against a real plugin
//! process: this package's own `veles-plugin-echo` binary.
//!
//! Each test builds a scratch home, installs the echo plugin into a
//! plugin root by writing its `plugin.toml`, starts a host over a test
//! bus, and drives the process through its `echo.*` methods.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};
use veles_core::bus::{GatewayBus, InboundMessage, MessageKind, OutboundMessage};
use veles_core::pluginhost::{HostOptions, InstanceStatus, LeaseGate, PluginHost, Ungated};
use veles_types::jsonrpc::RpcError;

const ECHO: &str = env!("CARGO_BIN_EXE_veles-plugin-echo");

struct Rig {
    _home: tempfile::TempDir,
    _env: veles_core::testenv::EnvGuard,
    host: PluginHost,
    bus: GatewayBus,
    inbound: mpsc::UnboundedReceiver<InboundMessage>,
}

fn fast() -> HostOptions {
    HostOptions {
        init_timeout: Duration::from_secs(10),
        shutdown_grace: Duration::from_secs(3),
        deliver_timeout: Duration::from_secs(10),
        backoff_base: Duration::from_millis(100),
        backoff_max: Duration::from_millis(400),
        stable_after: Duration::from_secs(5),
        queue_cap: 100,
        queue_ttl: Duration::from_secs(60),
    }
}

/// A scratch home with `config` as config.toml and the echo plugin
/// installed (plus any `extra` plugin dirs, name → entry script).
async fn rig_with(config: &str, extra: &[(&str, &str)], gate: Arc<dyn LeaseGate>) -> Rig {
    rig_opts(config, extra, gate, fast()).await
}

async fn rig_opts(
    config: &str,
    extra: &[(&str, &str)],
    gate: Arc<dyn LeaseGate>,
    opts: HostOptions,
) -> Rig {
    let (home, env) = veles_core::testenv::scratch_home();
    std::fs::write(veles_core::config::config_dir().join("config.toml"), config).unwrap();
    let root = home.path().join("process-plugins-under-test");
    let echo = root.join("echo");
    std::fs::create_dir_all(&echo).unwrap();
    std::fs::write(
        echo.join("plugin.toml"),
        format!("name = \"echo\"\nkind = \"process\"\nentry = \"{ECHO}\"\n"),
    )
    .unwrap();
    for (name, script) in extra {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        let exe = d.join("run.sh");
        std::fs::write(&exe, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            d.join("plugin.toml"),
            format!("name = \"{name}\"\nkind = \"process\"\nentry = \"run.sh\"\n"),
        )
        .unwrap();
    }
    let (tx, inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let host = PluginHost::new(bus.clone(), "node-under-test", opts);
    host.start(
        &[root],
        &[],
        &[],
        veles_core::pluginhost::raw_config_json(),
        gate,
    )
    .await;
    Rig {
        _home: home,
        _env: env,
        host,
        bus,
        inbound,
    }
}

async fn rig(config: &str) -> Rig {
    rig_with(config, &[], Arc::new(Ungated)).await
}

const ONE_ROOM: &str = "[[platforms.echo.rooms]]\nname = \"a\"\n";
const TWO_ROOMS: &str =
    "[[platforms.echo.rooms]]\nname = \"a\"\n\n[[platforms.echo.rooms]]\nname = \"b\"\n";

fn status(r: &Rig, label: &str) -> Option<InstanceStatus> {
    r.host.status().into_iter().find(|s| {
        let l = if s.instance.is_empty() {
            s.plugin.clone()
        } else {
            format!("{}:{}", s.plugin, s.instance)
        };
        l == label
    })
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn running(r: &Rig, label: &str) {
    until(&format!("{label} running"), || {
        status(r, label).is_some_and(|s| s.state == "running")
    })
    .await;
}

async fn pid(r: &Rig, room: &str) -> u64 {
    r.host
        .call("echo", room, "echo.pid", Value::Null)
        .await
        .unwrap()
        .as_u64()
        .unwrap()
}

fn state_file(room: &str, file: &str) -> String {
    std::fs::read_to_string(
        veles_core::config::data_dir()
            .join("plugin-state/echo")
            .join(room)
            .join(file),
    )
    .unwrap_or_default()
}

fn alive(pid: u64) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

async fn next_inbound(r: &mut Rig) -> Option<InboundMessage> {
    tokio::time::timeout(Duration::from_secs(5), r.inbound.recv())
        .await
        .ok()
        .flatten()
}

fn say(r: &Rig, room: &str, text: &str) {
    r.bus.send_outbound_to(
        "echo",
        Some(room),
        OutboundMessage::plain("c1", text, MessageKind::Reply),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_go_both_ways_through_the_plugin() {
    let mut r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    r.host
        .call(
            "echo",
            "a",
            "echo.say",
            json!({"chat_id": "c1", "sender": "s", "text": "hi", "sender_id": "echo:s"}),
        )
        .await
        .unwrap();
    let m = next_inbound(&mut r)
        .await
        .expect("the platform message never arrived");
    assert_eq!(m.platform, "echo");
    assert_eq!(m.route.as_deref(), Some("a"), "the route is the instance");
    assert_eq!(m.text, "hi");
    assert_eq!(m.sender_id.as_deref(), Some("echo:s"));
    say(&r, "a", "reply one");
    until("the reply delivered", || {
        state_file("a", "delivered.jsonl").contains("reply one")
    })
    .await;
}

/// A process that dies is started again, and works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_plugin_is_restarted() {
    let mut r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let before = pid(&r, "a").await;
    let _ = r
        .host
        .call("echo", "a", "echo.crash", json!({"code": 3}))
        .await;
    until("a restart", || {
        status(&r, "echo:a").is_some_and(|s| s.restarts >= 1 && s.state == "running")
    })
    .await;
    let after = pid(&r, "a").await;
    assert_ne!(before, after);
    let s = status(&r, "echo:a").unwrap();
    assert!(
        s.last_error.as_deref().unwrap_or("").contains("ended"),
        "{s:?}"
    );
    r.host
        .call("echo", "a", "echo.say", json!({"text": "back"}))
        .await
        .unwrap();
    assert_eq!(next_inbound(&mut r).await.unwrap().text, "back");
}

/// A reload asks the old process to shut down, then starts a new one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_replaces_the_process() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let before = pid(&r, "a").await;
    let out = r.host.reload("echo:a").await.unwrap();
    assert_eq!(out, vec!["echo:a: reloading".to_string()]);
    running(&r, "echo:a").await;
    let after = pid(&r, "a").await;
    assert_ne!(before, after);
    assert!(!alive(before), "the old process is still alive");
    assert!(
        state_file("a", "events.log").contains("shutdown reload"),
        "the old process was not asked to shut down: {}",
        state_file("a", "events.log")
    );
    assert_eq!(
        status(&r, "echo:a").unwrap().restarts,
        0,
        "a reload is not a crash"
    );
    say(&r, "a", "after the reload");
    until("delivery after the reload", || {
        state_file("a", "delivered.jsonl").contains("after the reload")
    })
    .await;
}

/// While an instance is stopped its messages wait, and go out in order
/// once it is started again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_wait_while_stopped_and_go_out_in_order() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let p = pid(&r, "a").await;
    r.host.stop("echo:a").await.unwrap();
    until("stopped", || {
        status(&r, "echo:a").is_some_and(|s| s.state == "stopped")
    })
    .await;
    assert!(!alive(p), "a stopped instance's process is still alive");
    for i in 1..=3 {
        say(&r, "a", &format!("queued {i}"));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!state_file("a", "delivered.jsonl").contains("queued 1"));
    r.host.start_again("echo:a").await.unwrap();
    until("the queue drained", || {
        state_file("a", "delivered.jsonl").contains("queued 3")
    })
    .await;
    let log = state_file("a", "delivered.jsonl");
    let at = |t: &str| log.find(t).unwrap();
    assert!(at("queued 1") < at("queued 2") && at("queued 2") < at("queued 3"));
}

/// Two instances are two processes: one crashing leaves the other alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_room_crashing_leaves_the_other_alone() {
    let r = rig(TWO_ROOMS).await;
    running(&r, "echo:a").await;
    running(&r, "echo:b").await;
    let b = pid(&r, "b").await;
    let _ = r.host.call("echo", "a", "echo.crash", json!({})).await;
    until("a restarted", || {
        status(&r, "echo:a").is_some_and(|s| s.restarts >= 1 && s.state == "running")
    })
    .await;
    assert_eq!(pid(&r, "b").await, b);
    assert_eq!(status(&r, "echo:b").unwrap().restarts, 0);
}

/// A plugin is given the secrets it declares and nothing else: not by
/// asking, and not through its environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_declared_secrets_reach_the_plugin() {
    // Set before the host spawns anything: the gateway's environment
    // holds both, as a real one holds every provider key.
    std::env::set_var("ECHO_TOKEN", "tok-123");
    std::env::set_var("OPENAI_API_KEY", "sk-must-not-leak");
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let got = r
        .host
        .call("echo", "a", "echo.secret", json!({"name": "ECHO_TOKEN"}))
        .await
        .unwrap();
    assert_eq!(got["value"], json!("tok-123"));
    let e = r
        .host
        .call(
            "echo",
            "a",
            "echo.secret",
            json!({"name": "OPENAI_API_KEY"}),
        )
        .await
        .unwrap_err();
    assert!(e.message.contains("does not declare"), "{e}");
    let env: Vec<String> = serde_json::from_value(
        r.host
            .call("echo", "a", "echo.env", Value::Null)
            .await
            .unwrap(),
    )
    .unwrap();
    for k in ["ECHO_TOKEN", "OPENAI_API_KEY", "VELES_HOME"] {
        assert!(
            !env.iter().any(|e| e == k),
            "{k} reached the plugin's environment"
        );
    }
    std::env::remove_var("ECHO_TOKEN");
    std::env::remove_var("OPENAI_API_KEY");
}

/// A plugin speaks for its own platform only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_cannot_speak_for_another_platform() {
    let mut r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    // A message for a platform it does not declare is dropped…
    r.host
        .call(
            "echo",
            "a",
            "echo.say",
            json!({"platform": "telegram", "text": "spoof"}),
        )
        .await
        .unwrap();
    // …a sender of another platform is not vouched for, and the context
    // names the real platform…
    r.host
        .call(
            "echo",
            "a",
            "echo.say",
            json!({"sender_id": "telegram:1", "text": "vouch", "route": "b", "ctx_platform": "telegram"}),
        )
        .await
        .unwrap();
    let m = next_inbound(&mut r).await.expect("the second message");
    assert_eq!(m.text, "vouch", "the spoofed message got through first");
    assert_eq!(m.sender_id, None);
    assert_eq!(m.sender_ctx.platform.as_deref(), Some("echo"));
    assert_eq!(
        m.route.as_deref(),
        Some("a"),
        "a plugin cannot pick another route"
    );
    // …and it may not ask who another platform's sender is.
    let e = r
        .host
        .call(
            "echo",
            "a",
            "echo.resolve",
            json!({"sender_id": "telegram:1"}),
        )
        .await
        .unwrap_err();
    assert!(e.message.contains("own platforms"), "{e}");
    let me = r
        .host
        .call(
            "echo",
            "a",
            "echo.resolve",
            json!({"sender_id": "echo:someone"}),
        )
        .await
        .unwrap();
    assert!(me.get("role").is_some(), "{me}");
}

/// A plugin that cannot start is retried with growing pauses, not in a
/// tight loop, and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_that_fails_to_start_backs_off() {
    let r = rig("[[platforms.echo.rooms]]\nname = \"a\"\nfail_start = true\n").await;
    until("two failed starts", || {
        status(&r, "echo:a").is_some_and(|s| s.restarts >= 2)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let s = status(&r, "echo:a").unwrap();
    assert!(
        s.last_error
            .as_deref()
            .unwrap_or("")
            .contains("configured to fail"),
        "{s:?}"
    );
    // 100 ms doubling to a 400 ms cap: about six attempts in two seconds.
    assert!(s.restarts <= 10, "{} restarts: no back-off", s.restarts);
}

/// A plugin built for another protocol is refused by name and never
/// started.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_protocol_is_refused_by_name() {
    let script = "#!/bin/sh\n\
        if [ \"$1\" = \"--describe\" ]; then\n\
          echo '{\"name\":\"old\",\"version\":\"0.9\",\"protocol\":99}'\n\
          exit 0\n\
        fi\n\
        echo 'started anyway' >&2\n\
        exit 1\n";
    let r = rig_with(ONE_ROOM, &[("old", script)], Arc::new(Ungated)).await;
    let s = status(&r, "old").expect("the old plugin is listed");
    assert_eq!(s.state, "refused");
    assert!(
        s.last_error
            .as_deref()
            .unwrap_or("")
            .contains("protocol 99"),
        "{s:?}"
    );
    running(&r, "echo:a").await;
}

/// A gate that holds the lease while a flag says so.
struct FlagGate(watch::Receiver<bool>);

#[async_trait::async_trait]
impl LeaseGate for FlagGate {
    async fn gated(
        &self,
        _key: &str,
        _what: &str,
        mut duty: Box<
            dyn FnMut() -> futures::future::BoxFuture<'static, anyhow::Result<()>> + Send,
        >,
    ) -> anyhow::Result<()> {
        let mut held = self.0.clone();
        loop {
            while !*held.borrow() {
                if held.changed().await.is_err() {
                    return Ok(());
                }
            }
            tokio::select! {
                r = duty() => return r,
                _ = async { while *held.borrow() { if held.changed().await.is_err() { break; } } } => {}
            }
        }
    }
}

/// Under a lease the process runs only while the lease is held, and a
/// lost lease kills it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_process_runs_only_while_its_lease_is_held() {
    let (tx, rx) = watch::channel(false);
    let r = rig_with(ONE_ROOM, &[], Arc::new(FlagGate(rx))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&r, "echo:a").unwrap().state, "standby");
    assert!(r
        .host
        .call("echo", "a", "echo.pid", Value::Null)
        .await
        .is_err());
    tx.send(true).unwrap();
    running(&r, "echo:a").await;
    let first = pid(&r, "a").await;
    tx.send(false).unwrap();
    until("standby after the lease went", || {
        status(&r, "echo:a").is_some_and(|s| s.state == "standby")
    })
    .await;
    until("the process gone", || !alive(first)).await;
    tx.send(true).unwrap();
    running(&r, "echo:a").await;
    assert_ne!(pid(&r, "a").await, first);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_target_names_what_exists() {
    let r = rig(ONE_ROOM).await;
    let e = r.host.reload("nope").await.unwrap_err();
    assert!(e.contains("echo:a"), "{e}");
    let e: RpcError = r
        .host
        .call("echo", "zz", "echo.pid", Value::Null)
        .await
        .unwrap_err();
    assert_eq!(e.code, RpcError::REFUSED);
}

/// The queue for a stopped instance keeps its newest messages up to the
/// cap; the oldest go first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_queue_drops_its_oldest() {
    let opts = HostOptions {
        queue_cap: 2,
        ..fast()
    };
    let r = rig_opts(ONE_ROOM, &[], Arc::new(Ungated), opts).await;
    running(&r, "echo:a").await;
    r.host.stop("echo:a").await.unwrap();
    for i in 1..=3 {
        say(&r, "a", &format!("capped {i}"));
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    r.host.start_again("echo:a").await.unwrap();
    until("the queue drained", || {
        state_file("a", "delivered.jsonl").contains("capped 3")
    })
    .await;
    let log = state_file("a", "delivered.jsonl");
    assert!(log.contains("capped 2"));
    assert!(
        !log.contains("capped 1"),
        "the oldest was kept past the cap"
    );
}

/// A message older than the queue's age limit is dropped, not delivered
/// late into a conversation that has moved on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stale_message_is_dropped_not_delivered_late() {
    let opts = HostOptions {
        queue_ttl: Duration::from_millis(300),
        ..fast()
    };
    let r = rig_opts(ONE_ROOM, &[], Arc::new(Ungated), opts).await;
    running(&r, "echo:a").await;
    r.host.stop("echo:a").await.unwrap();
    say(&r, "a", "too old");
    tokio::time::sleep(Duration::from_millis(700)).await;
    say(&r, "a", "fresh");
    r.host.start_again("echo:a").await.unwrap();
    until("the fresh one delivered", || {
        state_file("a", "delivered.jsonl").contains("fresh")
    })
    .await;
    assert!(!state_file("a", "delivered.jsonl").contains("too old"));
}

/// A standby instance (its lease is held elsewhere) must not claim its
/// platform on this node's bus: `has_adapter` is how the owner's asks
/// decide between asking here and relaying to the node that holds the
/// lease (`askrelay::OwnerRelayAsker`). A route registered at spawn made
/// every standby node answer "the adapter runs here".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_instance_does_not_claim_its_platform() {
    let (tx, rx) = watch::channel(false);
    let r = rig_with(ONE_ROOM, &[], Arc::new(FlagGate(rx))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&r, "echo:a").unwrap().state, "standby");
    assert!(
        !r.bus.has_adapter("echo"),
        "a standby instance claims the platform"
    );
    tx.send(true).unwrap();
    running(&r, "echo:a").await;
    assert!(r.bus.has_adapter("echo"), "the lease holder has no route");
    tx.send(false).unwrap();
    until("the route goes with the lease", || {
        !r.bus.has_adapter("echo")
    })
    .await;
}

/// A refusing gate (a pool member with no arbiter, a gateway that stops
/// when idle) leaves the instance listed as refused, with the reason —
/// not as "standby" forever, which reads as "waiting for the lease".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusing_gate_says_why() {
    struct No;
    #[async_trait::async_trait]
    impl LeaseGate for No {
        async fn gated(
            &self,
            _key: &str,
            _what: &str,
            _duty: veles_core::pluginhost::Duty,
        ) -> anyhow::Result<()> {
            anyhow::bail!("the singleton fence is refused on this node")
        }
    }
    let r = rig_with(ONE_ROOM, &[], Arc::new(No)).await;
    until("refused", || {
        status(&r, "echo:a").is_some_and(|s| s.state == "refused")
    })
    .await;
    let s = status(&r, "echo:a").unwrap();
    assert!(
        s.last_error.as_deref().unwrap_or("").contains("fence"),
        "{s:?}"
    );
    assert!(!r.bus.has_adapter("echo"));
}

/// The gateway stopping asks every plugin to shut down — an IRC plugin
/// sends its QUIT — and nothing comes back afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_gateway_stops_every_plugin_gracefully() {
    let r = rig(TWO_ROOMS).await;
    running(&r, "echo:a").await;
    running(&r, "echo:b").await;
    let (a, b) = (pid(&r, "a").await, pid(&r, "b").await);
    r.host.shutdown_all(Duration::from_secs(5)).await;
    for (room, p) in [("a", a), ("b", b)] {
        assert!(!alive(p), "room {room}'s process outlived the gateway");
        assert!(
            state_file(room, "events.log").contains("shutdown the gateway is stopping"),
            "room {room} was not asked to shut down: {}",
            state_file(room, "events.log")
        );
        assert_eq!(
            status(&r, &format!("echo:{room}")).unwrap().state,
            "stopped"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        r.host
            .call("echo", "a", "echo.pid", Value::Null)
            .await
            .is_err(),
        "a plugin came back after the gateway stopped"
    );
}

/// A command surface cannot wait on a reload (the gateway answers chat
/// commands from a sync dispatcher), so it files the request and
/// returns; the reload then happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_reload_returns_at_once_and_happens() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let before = pid(&r, "a").await;
    let labels = r.host.request("echo", "reload").unwrap();
    assert_eq!(labels, vec!["echo:a".to_string()]);
    until("a new process", || {
        status(&r, "echo:a").is_some_and(|s| s.state == "running" && s.pid != Some(before as u32))
    })
    .await;
    assert!(r.host.request("echo", "explode").is_err());
    assert!(r.host.request("nope", "reload").is_err());
}

/// `unload` (R7) ends the process AND the row: the instance leaves the
/// listing and the pid is gone, while the other room keeps running —
/// unlike `stop`, which keeps the row for a later `start`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unloaded_instance_leaves_the_listing_and_its_process_ends() {
    let r = rig(TWO_ROOMS).await;
    running(&r, "echo:a").await;
    running(&r, "echo:b").await;
    let pid_a = pid(&r, "a").await;
    let out = r.host.unload("echo:a").await.expect("unload");
    assert!(out.iter().any(|l| l.contains("unloaded")), "{out:?}");
    assert!(
        status(&r, "echo:a").is_none(),
        "the row is gone after an unload"
    );
    until("the process ended", || !alive(pid_a)).await;
    running(&r, "echo:b").await;
    assert!(
        r.host.request("echo:a", "start").is_err(),
        "an unloaded instance is not a target any more"
    );
    // The chat path files it and forgets the row at once.
    let labels = r.host.request("echo:b", "unload").unwrap();
    assert_eq!(labels, vec!["echo:b".to_string()]);
    assert!(status(&r, "echo:b").is_none());
}

/// Where an instance keeps its state is settled when the host is made,
/// not looked up again on every restart. The lookup reads a per-thread
/// override and the process env, and a restart runs on whichever worker
/// thread is free: in the suite that was after a finished test had put
/// both back, and a restarting echo wrote `plugin-state/echo/a` into
/// the real `~/.local/share/veles`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_state_dir_the_host_was_made_with() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let made_with = veles_core::config::data_dir().join("plugin-state/echo/a");
    let before = pid(&r, "a").await;
    let elsewhere = tempfile::tempdir().unwrap();
    let home = std::env::var_os("VELES_HOME");
    std::env::set_var("VELES_HOME", elsewhere.path());
    let moved = veles_core::config::set_dirs(Some(veles_core::config::DirOverride::forced(
        elsewhere.path(),
    )));
    r.host.request("echo:a", "reload").unwrap();
    until("a new process", || {
        status(&r, "echo:a").is_some_and(|s| s.state == "running" && s.pid != Some(before as u32))
    })
    .await;
    let after = pid(&r, "a").await;
    drop(moved);
    if let Some(h) = home {
        std::env::set_var("VELES_HOME", h);
    }
    let events = std::fs::read_to_string(made_with.join("events.log")).unwrap_or_default();
    assert!(
        events.contains(&format!("start {after}")),
        "the restarted process did not start in the host's state dir: {events:?}"
    );
    assert!(
        !elsewhere.path().join("plugin-state").exists(),
        "a restart followed the ambient env to another state dir"
    );
}

/// A plugin whose `--describe` the test rewrites between runs, standing
/// in for a binary updated in place. It answers `initialize` with the
/// same manifest, logs every `initialize` it gets to `init.log`, then
/// waits for EOF.
const UPDATABLE: &str = r#"#!/bin/sh
here=$(dirname "$0")
m='{"name":"upd","version":"1","protocol":1,"platforms":[{"key":"upd"}]}'
[ -f "$here/manifest.json" ] && m=$(cat "$here/manifest.json")
if [ "$1" = "--describe" ]; then printf '%s\n' "$m"; exit 0; fi
IFS= read -r line
printf '%s\n' "$line" >> "$here/init.log"
id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$m"
exec cat >/dev/null
"#;

/// Install version 2 of [`UPDATABLE`] and reload it.
async fn update_and_reload(r: &Rig, v2: &str) -> std::path::PathBuf {
    let dir = r._home.path().join("process-plugins-under-test/upd");
    std::fs::write(dir.join("manifest.json"), v2).unwrap();
    r.host.request("upd", "reload").unwrap();
    dir
}

/// A reload runs whatever binary is at the path now, so the host asks
/// it what it is before talking to it. `initialize` carries the config
/// sections, and `secret.get` checks the secrets, of the manifest the
/// host holds. Taken from the version the gateway started with, they
/// gave an update that added a section nothing on its first run, and
/// would refuse it a secret it had just declared.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_takes_the_new_versions_manifest() {
    let config = format!("{ONE_ROOM}\n[platforms.upd]\nflavour = \"new\"\n");
    let r = rig_with(&config, &[("upd", UPDATABLE)], Arc::new(Ungated)).await;
    running(&r, "upd").await;
    let before = status(&r, "upd").unwrap().pid;
    let dir = update_and_reload(
        &r,
        r#"{"name":"upd","version":"2","protocol":1,"platforms":[{"key":"upd"}],"config_sections":["platforms.upd"]}"#,
    )
    .await;
    until("version 2 running", || {
        status(&r, "upd").is_some_and(|s| s.state == "running" && s.pid != before)
    })
    .await;
    let inits = std::fs::read_to_string(dir.join("init.log")).unwrap();
    let first_of_v2 = inits.lines().last().unwrap();
    assert!(
        first_of_v2.contains(r#""flavour":"new""#),
        "version 2's first initialize lacked the section it declares: {first_of_v2}"
    );
    assert_eq!(status(&r, "upd").unwrap().version, "2");
}

/// What the host settled when it spawned the instance — the lease it
/// holds, the platforms it routes — cannot change under a running
/// gateway. A version that changes them must not run under the old
/// ones; the owner is told a restart takes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_that_moves_the_platforms_says_restart() {
    let r = rig_with(ONE_ROOM, &[("upd", UPDATABLE)], Arc::new(Ungated)).await;
    running(&r, "upd").await;
    update_and_reload(
        &r,
        r#"{"name":"upd","version":"2","protocol":1,"platforms":[{"key":"elsewhere"}]}"#,
    )
    .await;
    until("the reason", || {
        status(&r, "upd").is_some_and(|s| {
            s.state != "running"
                && s.last_error
                    .as_deref()
                    .is_some_and(|e| e.contains("platforms") && e.contains("starts again"))
        })
    })
    .await;
    assert!(
        !r.bus.has_adapter("elsewhere"),
        "the new platform was routed without a restart"
    );
}

/// The same for the lease: a version that names another one would run
/// fenced by the key it no longer uses.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reload_that_moves_the_lease_says_restart() {
    let r = rig_with(ONE_ROOM, &[("upd", UPDATABLE)], Arc::new(Ungated)).await;
    running(&r, "upd").await;
    update_and_reload(
        &r,
        r#"{"name":"upd","version":"2","protocol":1,"platforms":[{"key":"upd"}],"lease":"singleton:upd"}"#,
    )
    .await;
    until("the reason", || {
        status(&r, "upd").is_some_and(|s| {
            s.state != "running"
                && s.last_error
                    .as_deref()
                    .is_some_and(|e| e.contains("singleton:upd") && e.contains("starts again"))
        })
    })
    .await;
}

/// What a plugin writes to stderr is kept, per instance, for
/// `veles plugins logs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugins_log_lines_are_kept() {
    let r = rig(TWO_ROOMS).await;
    running(&r, "echo:a").await;
    running(&r, "echo:b").await;
    until("room a's start line", || {
        r.host.logs("echo:a").is_ok_and(|v| {
            v.iter()
                .any(|(_, l)| l.iter().any(|x| x.contains("echo room a up")))
        })
    })
    .await;
    let a = r.host.logs("echo:a").unwrap();
    assert_eq!(a.len(), 1);
    assert!(
        !a[0].1.iter().any(|x| x.contains("echo room b up")),
        "room b's line is in room a's log"
    );
    assert!(r.host.logs("nope").is_err());
}

/// The JSON-RPC envelope for a direct conversation with the binary.
fn rpc(id: u64, method: &str, params: Value) -> String {
    format!(
        "{}\n",
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    )
}

/// Talk to the echo binary with no host at all: initialize, then let the
/// "gateway" vanish (its end of stdin closes, as when it is SIGKILLed).
async fn orphaned(room: serde_json::Value) -> (tokio::process::Child, tempfile::TempDir) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(ECHO)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap()).lines();
    let init = json!({
        "protocol": veles_types::plugin::PROTOCOL,
        "host_version": "test",
        "node_id": "n",
        "plugin": "echo",
        "instance": "a",
        "config": {"platforms.echo": {"rooms": [room]}},
        "dirs": {"state": dir.path(), "cache": dir.path()},
    });
    stdin
        .write_all(rpc(1, "initialize", init).as_bytes())
        .await
        .unwrap();
    let answer = tokio::time::timeout(Duration::from_secs(10), out.next_line())
        .await
        .expect("no answer to initialize")
        .unwrap()
        .unwrap();
    assert!(answer.contains("\"result\""), "{answer}");
    drop(stdin);
    // Keep reading so a write from the plugin never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = out.next_line().await {} });
    (child, dir)
}

/// A gateway that dies without a word (SIGKILL, OOM) closes the plugin's
/// stdin. The plugin must notice, shut down, and exit — a leftover IRC
/// process would keep a second bot on the network next to the one the
/// restarted gateway starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_exits_when_the_gateway_goes_away() {
    let (mut child, dir) = orphaned(json!({"name": "a"})).await;
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("the orphaned plugin is still running")
        .unwrap();
    assert!(status.success(), "{status}");
    let events = std::fs::read_to_string(dir.path().join("events.log")).unwrap_or_default();
    assert!(
        events.contains("shutdown the gateway went away"),
        "it exited without shutting down: {events}"
    );
}

/// ... and a plugin whose own shutdown hangs still exits, on the SDK's
/// time limit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hung_shutdown_does_not_keep_an_orphan_alive() {
    let (mut child, _dir) = orphaned(json!({"name": "a", "hang_shutdown": true})).await;
    let limit = veles_plugin_sdk::SHUTDOWN_LIMIT + Duration::from_secs(5);
    let status = tokio::time::timeout(limit, child.wait())
        .await
        .expect("a hung shutdown kept the orphan alive")
        .unwrap();
    assert!(
        !status.success(),
        "a cut-short shutdown should not report success: {status}"
    );
}

/// An instance on standby when the gateway stops has no supervisor to
/// hear the shutdown; if its lease is granted in the last seconds, it
/// must not start a process the exiting gateway then kills mid-connect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_instance_stays_down_once_the_gateway_stops() {
    let (tx, rx) = watch::channel(false);
    let r = rig_with(ONE_ROOM, &[], Arc::new(FlagGate(rx))).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(status(&r, "echo:a").unwrap().state, "standby");
    r.host.shutdown_all(Duration::from_millis(300)).await;
    tx.send(true).unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_file("a", "events.log").contains("start"),
        "a process started after the gateway stopped: {}",
        state_file("a", "events.log")
    );
}

/// The Phase 3 guard rails, driven through a real plugin: the gateway is
/// the only authority on the ACL and on config, and a plugin reaches them
/// only as its manifest allows.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_grant_for_a_foreign_platform_is_refused() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    // echo declares only the platform `echo`; an `irc:` identity is not its
    // to grant.
    let v = r
        .host
        .call(
            "echo",
            "a",
            "echo.grant",
            json!({"identity": "irc:pal", "role": "user"}),
        )
        .await
        .unwrap();
    assert!(
        v.get("refused").is_some(),
        "a foreign-platform grant must be refused: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_is_never_grantable_by_a_plugin() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    let v = r
        .host
        .call(
            "echo",
            "a",
            "echo.grant",
            json!({"identity": "echo:s", "role": "owner"}),
        )
        .await
        .unwrap();
    assert!(
        v.get("refused").is_some(),
        "owner must never be grantable: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_get_reads_only_a_declared_section() {
    let r = rig(ONE_ROOM).await;
    running(&r, "echo:a").await;
    // echo declares `platforms.echo`; anything else is refused.
    let denied = r
        .host
        .call("echo", "a", "echo.config_get", json!({"path": "acl"}))
        .await
        .unwrap();
    assert!(
        denied.get("refused").is_some(),
        "an undeclared section must be refused: {denied}"
    );
    let ok = r
        .host
        .call(
            "echo",
            "a",
            "echo.config_get",
            json!({"path": "platforms.echo"}),
        )
        .await
        .unwrap();
    assert!(
        ok.get("value").is_some(),
        "the declared section reads: {ok}"
    );
}

/// A process plugin PROVIDES the services its manifest declares while it
/// runs (answered by its `request` handler) and CALLS others through the
/// gateway (`service.call`): an open service answers, an internal one is
/// refused, and the caller is named. Stopped, its services go.
///
/// The registry is process-global and other tests run echo rooms too, so
/// this room has its own name and the assertions name its own owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_plugin_provides_and_calls_services() {
    use veles_core::services;
    let r = rig("[[platforms.echo.rooms]]\nname = \"svc\"\n").await;
    running(&r, "echo:svc").await;
    until("echo.upper provided by this room", || {
        services::providers("echo.upper").contains(&"process:echo:svc".to_string())
    })
    .await;
    let v = services::call("echo.upper", json!({"text": "hi"}))
        .await
        .unwrap();
    assert_eq!(v, json!({"text": "HI"}));

    let owner = veles_core::lifecycle::owner_tag("module:echo-host-test");
    let decl = |name: &str, cap: Option<&str>| services::ServiceDecl {
        name: name.into(),
        description: String::new(),
        cap: cap.map(str::to_string),
    };
    services::register_for(
        owner,
        decl("echotest.open", Some("any")),
        services::handler(|p, cx| async move { Ok(json!({"got": p, "caller": cx.caller})) }),
    )
    .unwrap();
    services::register_for(
        owner,
        decl("echotest.internal", None),
        services::handler(|_p, _cx| async move { Ok(json!("secret")) }),
    )
    .unwrap();
    let v = r
        .host
        .call(
            "echo",
            "svc",
            "echo.service_call",
            json!({"name": "echotest.open", "params": {"x": 1}}),
        )
        .await
        .unwrap();
    assert_eq!(v["got"]["x"], 1, "{v}");
    assert_eq!(v["caller"], "process:echo:svc", "{v}");
    let v = r
        .host
        .call(
            "echo",
            "svc",
            "echo.service_call",
            json!({"name": "echotest.internal", "params": {}}),
        )
        .await
        .unwrap();
    assert!(
        v["refused"]
            .as_str()
            .unwrap_or_default()
            .contains("internal service"),
        "{v}"
    );
    services::purge_owner(Some(owner));

    // The dependency view knows what echo declares.
    let row = services::report()
        .into_iter()
        .find(|r| r.name == "echotest.open")
        .unwrap();
    assert!(
        row.used_by
            .iter()
            .any(|(p, optional)| p == "echo" && *optional),
        "{row:?}"
    );

    r.host.request("echo:svc", "stop").unwrap();
    until("its services withdrawn", || {
        !services::providers("echo.upper").contains(&"process:echo:svc".to_string())
    })
    .await;
}

/// A process plugin overrides a point (PLAN-extension-api.md D2): its
/// request is the envelope `{input, next, caller, session_id}`, and
/// `service.next` with that token answers what the layer below would.
/// The point is declared AFTER the plugin registered its override: the
/// default still goes under it.
///
/// Other tests run echo rooms too, and every room provides the point, so
/// the answer is held to its shape (core's first, then one "process" per
/// echo layer) and the stop to this room's own provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_plugin_overrides_a_point_and_wraps_next() {
    use veles_core::services;
    let r = rig("[[platforms.echo.rooms]]\nname = \"pt\"\n").await;
    running(&r, "echo:pt").await;
    until("echotest.point provided by this room", || {
        services::providers("echotest.point").contains(&"process:echo:pt".to_string())
    })
    .await;
    let owner = veles_core::lifecycle::owner_tag("module:echo-point-test");
    veles_core::lifecycle::with_thread_owner(owner, || {
        services::declare_point(
            services::PointDecl {
                name: "echotest.point".into(),
                summary: "the echo host test's point".into(),
                input: json!({"type": "object"}),
                output: json!({"type": "object", "required": ["via"]}),
                on_error: services::OnError::Fail,
                deadline_ms: 20_000,
            },
            services::handler(|_p, _cx| async { Ok(json!({"via": ["core"]})) }),
        )
    })
    .unwrap();
    assert!(services::overridden("echotest.point"));
    let v = services::ask("echotest.point", json!({}), None)
        .await
        .unwrap();
    let via: Vec<&str> = v["via"]
        .as_array()
        .unwrap_or_else(|| panic!("no via list: {v}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(via.first(), Some(&"core"), "{v}");
    assert!(
        via.len() >= 2 && via[1..].iter().all(|t| *t == "process"),
        "each echo layer wrapped what `next` answered: {v}"
    );

    r.host.request("echo:pt", "stop").unwrap();
    until("its override withdrawn", || {
        !services::providers("echotest.point").contains(&"process:echo:pt".to_string())
    })
    .await;
    services::purge_owner(Some(owner));
}
