// A test/example driver: it spawns onto a scratch runtime it owns;
// the M3 hop (clippy.toml) is for production core/frontends code.
#![allow(clippy::disallowed_methods)]

//! A plugin that fails to start the same way on every try (an
//! unconfigured platform: the atproto plugin on a fresh server) is
//! retried on the back-off, but says so ONCE. Every restart used to park
//! a "plugin … ended; restarting" note, and the next chat reply carried
//! up to 32 of them.
//!
//! Its own test binary: plugin notes are a process-wide lot (capped),
//! so a count is only meaningful where no other test parks notes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use veles_core::bus::GatewayBus;
use veles_core::pluginhost::{HostOptions, PluginHost, Ungated};

const ECHO: &str = env!("CARGO_BIN_EXE_veles-plugin-echo");

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_identical_start_failure_is_announced_once() {
    let (home, _env) = veles_core::testenv::scratch_home();
    std::fs::write(
        veles_core::config::config_dir().join("config.toml"),
        "[[platforms.echo.rooms]]\nname = \"a\"\nfail_start = true\n",
    )
    .unwrap();
    let root = home.path().join("process-plugins-under-test");
    std::fs::create_dir_all(root.join("echo")).unwrap();
    std::fs::write(
        root.join("echo/plugin.toml"),
        format!("name = \"echo\"\nkind = \"process\"\nentry = \"{ECHO}\"\n"),
    )
    .unwrap();
    let (tx, _inbound) = mpsc::unbounded_channel();
    let bus = GatewayBus {
        inbound: tx,
        outbound: Arc::new(Mutex::new(HashMap::new())),
        last_chat: Arc::new(Mutex::new(HashMap::new())),
        state_path: home.path().join("gateway-state.json"),
    };
    let opts = HostOptions {
        init_timeout: Duration::from_secs(10),
        shutdown_grace: Duration::from_secs(3),
        deliver_timeout: Duration::from_secs(10),
        backoff_base: Duration::from_millis(50),
        backoff_max: Duration::from_millis(100),
        stable_after: Duration::from_secs(5),
        queue_cap: 100,
        queue_ttl: Duration::from_secs(60),
    };
    let host = PluginHost::new(bus, "node-under-test", opts);
    host.start(
        &[root],
        &[],
        &[],
        veles_core::pluginhost::raw_config_json(),
        Arc::new(Ungated),
    )
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let s = host
            .status()
            .into_iter()
            .find(|s| s.plugin == "echo" && s.instance == "a");
        if s.as_ref().is_some_and(|s| s.restarts >= 4) {
            let s = s.unwrap();
            // Still retried, and the status still says why.
            assert!(
                s.last_error
                    .as_deref()
                    .unwrap_or("")
                    .contains("configured to fail"),
                "{s:?}"
            );
            break;
        }
        assert!(std::time::Instant::now() < deadline, "no restarts: {s:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let notes: Vec<_> = veles_core::notes::take_plugin_notes()
        .into_iter()
        .map(|n| n.summary)
        .filter(|s| s.contains("echo:a"))
        .collect();
    assert_eq!(notes.len(), 1, "one note for one reason: {notes:?}");
    host.shutdown_all(Duration::from_secs(5)).await;
}
