//! The Telegram platform as an in-process MODULE (PLAN-plugins-installer.md
//! R6): the host-facing shell — the manifest, `start(cx)`, `stop`. The
//! module FILE is crates/veles-module-telegram, which exports this;
//! veles-telegram itself is a plain library the gateway links for its
//! config surfaces and never loads as a module (the R4 lesson).
//!
//! WHERE THE LOOP RUNS. teloxide's `dispatch` blocks its caller in a
//! `std::thread::scope` running a nested runtime (teloxide#1154), so
//! the linked adapter had an OS thread and a runtime of its own. Here
//! the loop runs on the INSTANCE runtime's blocking pool
//! (`spawn_blocking` + `Handle::block_on`): a host-owned, owner-tagged,
//! counted thread, so the quiescence check sees it, while the runtime's
//! worker stays free for the outbound consumer and the timers. The
//! nested runtime's threads are teloxide's own and end when `dispatch`
//! returns (the scope joins them).
//!
//! HOW IT ENDS. `stop` ends the long poll through the dispatcher's
//! `ShutdownToken` (the adapter hands it out once the dispatcher
//! exists) and waits, bounded, until the blocking task has RETURNED;
//! teardown then finds nothing left. Without the token the blocking
//! task would outlive `shutdown_timeout` and the module would be kept
//! mapped — honest, but a leak per reload.
//!
//! A stop can come before the poll runs: push-plugin's rescan started
//! the VPS module and its reload stopped it 70 ms later (2026-10-10).
//! The token refuses while the dispatcher is idle, and idle is also the
//! moment between handing the token out and `dispatch` running. The old
//! stop took that refusal for "already stopped" and returned; teardown
//! shut the runtime down under the live poll, whose timer panicked ("A
//! Tokio 1.x context was found, but it is being shutdown"). So `stop`
//! sets `StopSlot::stopping` first (the loop checks it before `get_me`
//! and before `dispatch`), asks the token again until it takes, and
//! waits for the task's end.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use veles_runtime::anyhow;
use veles_runtime::veles_core;
use veles_runtime::veles_core::rustplugins::{ModuleContext, VelesPlugin};
use veles_runtime::veles_types::plugin::{Instances, Manifest, PlatformDecl};

/// How long `stop` waits for the dispatcher to end after the token.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// How often `stop` asks an idle dispatcher again (see the module doc).
const STOP_RETRY: std::time::Duration = std::time::Duration::from_millis(25);

/// One instance: the bot. Holds the stop slot the loop reads, and the
/// end of the loop's blocking task.
#[derive(Default)]
pub struct TelegramModule {
    slot: Arc<Mutex<crate::StopSlot>>,
    ended: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

/// The manifest the module exports (crates/veles-module-telegram).
pub fn manifest() -> Manifest {
    Manifest {
        name: "telegram".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        protocol: 1,
        description: "Telegram platform (teloxide): the bot's long poll, replies, asks and \
                      media; one instance per gateway."
            .into(),
        instances: Instances::Single,
        // The SAME key the linked adapter held (`arbiter::lease_key_telegram`),
        // so a mixed-version pool arbitrates the one poller between a
        // module node and a linked one.
        lease: Some("singleton:telegram".into()),
        platforms: vec![PlatformDecl {
            key: "telegram".into(),
            public_feed: false,
        }],
        config_sections: vec!["platforms.telegram".into(), "display".into()],
        knobs: crate::config_surface(),
        secrets: Vec::new(),
        services: Vec::new(),
        uses: Vec::new(),
        hooks: Vec::new(),
        enabled_env: None,
        // No token on a node, no instance there: the linked adapter's
        // `try_from_config` answered None; the host answers before any
        // lease is asked for.
        required_env: Some("TELEGRAM_BOT_TOKEN".into()),
        points: crate::points::point_decls(),
    }
}

impl VelesPlugin for TelegramModule {
    fn name(&self) -> &str {
        "telegram"
    }

    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }

    fn start(
        &self,
        cx: ModuleContext,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            // The adapter reads the whole config (the platform block and
            // the display profile), as it always did.
            let cfg = veles_core::config::load()?;
            let Some(adapter) = crate::TelegramAdapter::try_from_config(&cfg) else {
                anyhow::bail!(
                    "telegram is not configured on this node ([platforms.telegram] enabled, \
                     TELEGRAM_BOT_TOKEN set)"
                );
            };
            // The override point this module declares
            // (PLAN-extension-api.md E4): whether a group message is for
            // the bot. Under the instance's owner, every start.
            crate::points::declare().map_err(|e| anyhow::anyhow!("{e}"))?;
            let bus = cx.bus.clone();
            let handle = cx.handle.clone();
            let slot = self.slot.clone();
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = crate::StopSlot::default();
            // Dropped when the blocking task ends, by return or by panic:
            // `stop` waits on it.
            let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
            *self.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(ended_rx);
            // The long poll, on the instance runtime's BLOCKING pool (see
            // the module doc): teardown's `shutdown_timeout` waits for it,
            // and `stop` makes it end first.
            cx.handle.spawn_blocking(move || {
                let _ended = ended_tx;
                let r = handle.block_on(adapter.run_with_shutdown(bus, Some(slot)));
                let line = match &r {
                    Ok(()) => "telegram: adapter loop ended".to_string(),
                    Err(e) => format!("telegram: adapter loop failed: {e:#}"),
                };
                veles_core::module::log(
                    if r.is_ok() {
                        veles_runtime::tracing::Level::INFO
                    } else {
                        veles_runtime::tracing::Level::ERROR
                    },
                    &line,
                );
            });
            Ok(())
        })
    }

    fn stop(&self, reason: &str) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let reason = reason.to_string();
        let slot = self.slot.clone();
        // First, so a loop that has no dispatcher yet ends at its next
        // check instead of starting a poll.
        slot.lock().unwrap_or_else(|e| e.into_inner()).stopping = true;
        let ended = self.ended.lock().unwrap_or_else(|e| e.into_inner()).take();
        Box::pin(async move {
            let t0 = std::time::Instant::now();
            let Some(mut ended) = ended else {
                veles_core::module::log(
                    veles_runtime::tracing::Level::INFO,
                    &format!("telegram: stop (\"{reason}\"): no adapter loop was running"),
                );
                return;
            };
            let deadline = tokio::time::Instant::now() + STOP_WAIT;
            let mut asked = false;
            let ended_in_time = loop {
                if !asked {
                    let token = slot.lock().unwrap_or_else(|e| e.into_inner()).token.clone();
                    // `Err` = idle: not dispatching YET, or not any more.
                    // The task's end tells which; until then, ask again.
                    asked = token.is_some_and(|t| t.shutdown().is_ok());
                }
                let until = if asked {
                    deadline
                } else {
                    deadline.min(tokio::time::Instant::now() + STOP_RETRY)
                };
                if tokio::time::timeout_at(until, &mut ended).await.is_ok() {
                    break true;
                }
                if tokio::time::Instant::now() >= deadline {
                    break false;
                }
            };
            veles_core::module::log(
                veles_runtime::tracing::Level::INFO,
                &format!(
                    "telegram: stop (\"{reason}\"): {} ({:?})",
                    if ended_in_time {
                        "the adapter loop ended"
                    } else {
                        "the adapter loop did not end within the deadline"
                    },
                    t0.elapsed()
                ),
            );
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_keeps_the_linked_lease_and_requires_the_token() {
        let m = manifest();
        assert_eq!(m.name, "telegram");
        assert_eq!(m.lease.as_deref(), Some("singleton:telegram"));
        assert_eq!(
            m.lease.as_deref().map(String::from),
            Some(veles_core::arbiter::lease_key_telegram()),
            "the module and the linked adapter arbitrate the same lease"
        );
        assert_eq!(m.platforms.len(), 1);
        assert_eq!(m.platforms[0].key, "telegram");
        assert_eq!(m.required_env.as_deref(), Some("TELEGRAM_BOT_TOKEN"));
        assert_eq!(m.instances, Instances::Single);
    }

    /// With no dispatcher ever started, `stop` is immediate (a refused
    /// load must not wait out the deadline).
    #[tokio::test]
    async fn stop_without_a_dispatcher_returns_at_once() {
        let m = TelegramModule::default();
        let t0 = std::time::Instant::now();
        m.stop("test").await;
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
    }
}

#[cfg(test)]
mod installer_surface_tests {
    #[test]
    fn manifest_carries_the_telegram_config_surface() {
        let encoded = serde_json::to_value(super::manifest()).expect("manifest serializes");
        let actual: Vec<veles_runtime::veles_types::Knob> =
            serde_json::from_value(encoded["knobs"].clone()).expect("module exposes knobs");
        assert_eq!(actual, crate::config_surface());
        assert!(actual
            .iter()
            .any(|k| k.path == "platforms.telegram.enabled"));
    }
}
