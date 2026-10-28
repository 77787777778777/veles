//! Engine logs, captured instead of scribbled over the UI.
//!
//! The TUI owns the terminal through ratatui's alternate screen, but
//! `tracing_subscriber::fmt()`'s default writer is **stdout** — the same
//! handle. Every `tracing::info!` on the turn path (routing, compaction,
//! MCP connects, ask timeouts) therefore wrote raw unpositioned bytes
//! into the alt-screen buffer and shredded the layout. That is what the
//! user saw as "status messages corrupting the layout"; the TUI's own
//! status lines were never the problem.
//!
//! So: one init for the whole process, and under an interactive UI the
//! logs go to a ring buffer (rendered in a log pane / served to the
//! desktop over REST) plus a file, and NOT to stdout.
//!
//! `init` is idempotent by design. Two different call sites used to
//! install a global subscriber under different conditions, so
//! `veles chat -q "hi"` panicked with "a global default trace dispatcher
//! has already been set" — the whole non-quiet one-shot path was dead.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// How many lines the ring keeps. Enough to cover a long turn's worth of
/// engine chatter; the file has the rest.
pub const CAP: usize = 500;

#[derive(Debug, Clone, serde::Serialize)]
pub struct LogLine {
    pub time: String,
    pub level: String,
    pub target: String,
    pub msg: String,
}

fn buf() -> &'static Mutex<VecDeque<LogLine>> {
    static BUF: OnceLock<Mutex<VecDeque<LogLine>>> = OnceLock::new();
    BUF.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAP)))
}

pub fn push(line: LogLine) {
    let Ok(mut b) = buf().lock() else { return };
    if b.len() == CAP {
        b.pop_front();
    }
    b.push_back(line);
}

/// Most recent `limit` lines, oldest first (render order).
pub fn snapshot(limit: usize) -> Vec<LogLine> {
    let Ok(b) = buf().lock() else {
        return Vec::new();
    };
    let skip = b.len().saturating_sub(limit);
    b.iter().skip(skip).cloned().collect()
}

pub fn len() -> usize {
    buf().lock().map(|b| b.len()).unwrap_or(0)
}

pub fn clear() {
    if let Ok(mut b) = buf().lock() {
        b.clear();
    }
}

// ── the tracing layer ────────────────────────────────────────────────

#[derive(Default)]
struct FieldVisitor {
    msg: String,
    extra: Vec<String>,
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.msg = format!("{value:?}");
            // Debug on a str-ish message double-quotes it; unwrap that.
            if self.msg.len() >= 2 && self.msg.starts_with('"') && self.msg.ends_with('"') {
                self.msg = self.msg[1..self.msg.len() - 1].replace("\\\"", "\"");
            }
        } else {
            self.extra.push(format!("{}={value:?}", field.name()));
        }
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.msg = value.to_string();
        } else {
            self.extra.push(format!("{}={value}", field.name()));
        }
    }
}

/// Writes every event into the ring buffer.
pub struct RingLayer;

/// A span's own fields, formatted `name=value` and parked in its
/// extensions at `new_span`/`on_record`, because the registry keeps no
/// record of a span's VALUES otherwise — a `SpanRef` answers the field
/// SET, not what each was set to. The fmt layer does exactly this for
/// stdout; the ring keeps its own copy so the two never share a
/// representation, and so the ring works when no fmt layer is installed
/// (the quiet one-shot path, which installs RingLayer alone).
struct SpanFields(Vec<String>);

impl<S> Layer<S> for RingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let span = ctx.span(id).expect("span not found at new_span");
        let mut v = FieldVisitor::default();
        attrs.record(&mut v);
        span.extensions_mut().insert(SpanFields(v.extra));
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        // A span that records more fields later gets them folded in.
        let span = ctx.span(id).expect("span not found at on_record");
        let mut v = FieldVisitor::default();
        values.record(&mut v);
        let mut ext = span.extensions_mut();
        match ext.get_mut::<SpanFields>() {
            Some(existing) => existing.0.extend(v.extra),
            None => {
                drop(ext);
                span.extensions_mut().insert(SpanFields(v.extra));
            }
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut v = FieldVisitor::default();
        event.record(&mut v);
        let mut msg = v.msg;
        if !v.extra.is_empty() {
            if !msg.is_empty() {
                msg.push(' ');
            }
            msg.push_str(&v.extra.join(" "));
        }
        // The enclosing spans' fields, so a line answers "which network /
        // which presence" without every call site repeating it (the IRC
        // connection holds one `info_span!("irc", network, presence)` for
        // its life; two networks' lines were indistinguishable before).
        // Outermost first, only fields the event did not already carry.
        if let Some(scope) = ctx.event_scope(event) {
            let mut spans: Vec<String> = Vec::new();
            for span in scope.from_root() {
                let ext = span.extensions();
                if let Some(SpanFields(fields)) = ext.get::<SpanFields>() {
                    for field in fields {
                        if !v.extra.contains(field) && !spans.contains(field) {
                            spans.push(field.clone());
                        }
                    }
                }
            }
            if !spans.is_empty() {
                if !msg.is_empty() {
                    msg.push(' ');
                }
                msg.push_str(&format!("[{}]", spans.join(" ")));
            }
        }
        let meta = event.metadata();
        push(LogLine {
            time: chrono::Local::now().format("%H:%M:%S%.3f").to_string(),
            level: meta.level().to_string(),
            target: meta.target().to_string(),
            msg,
        });
    }
}

// ── one init for the process ─────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    /// Ordinary CLI: human-readable logs on stdout.
    Stdout,
    /// Quiet one-shot (`chat -q -Q`): stdout is the machine contract, so
    /// logs go to stderr (delegate_task depends on this).
    Stderr,
    /// An interactive UI owns the terminal: ring buffer + file, and
    /// nothing on stdout.
    Captured,
    /// A person ran a one-off command (`doctor`, `secret list`, `setup`,
    /// the plain REPL): the console shows warnings and errors only, on
    /// stderr, unless RUST_LOG asks for more. The ring still records
    /// everything at info, and the GLOBAL filter stays at info so module
    /// callsites keep registering (see `report` below). Before this every
    /// command opened with the in-tree plugins' boot lines on stdout.
    Quiet,
}

static INITIALISED: OnceLock<LogMode> = OnceLock::new();

/// The filter `RUST_LOG` asks for; unset and EMPTY both mean "info".
///
/// An empty variable parses as "no directives", whose max level is
/// OFF: every log line vanishes, and so does every tracing callsite —
/// which is how the flake's sandbox (nixpkgs' rust builder exports
/// `RUST_LOG=`) failed the irc-module case for days while every local
/// run passed (2026-10-13). `off` is still the explicit way to say off;
/// a directive that does not parse falls back to "info" as before.
fn filter_from(rust_log: Option<String>) -> tracing_subscriber::EnvFilter {
    match rust_log {
        Some(v) if !v.trim().is_empty() => {
            tracing_subscriber::EnvFilter::try_new(v).unwrap_or_else(|_| "info".into())
        }
        _ => "info".into(),
    }
}

/// Install the global subscriber. Idempotent: the first call wins and
/// later ones are ignored (they used to panic).
pub fn init(mode: LogMode) -> LogMode {
    let mut chosen = mode;
    let mut first = false;
    let _ = INITIALISED.get_or_init(|| {
        first = true;
        mode
    });
    if !first {
        chosen = *INITIALISED.get().unwrap_or(&mode);
        return chosen;
    }
    use tracing_subscriber::prelude::*;
    let filter = filter_from(std::env::var("RUST_LOG").ok());
    // The ring is installed in EVERY mode: it costs a bounded buffer,
    // and the desktop reads it over REST from the GATEWAY process —
    // which runs in Stdout mode, not Captured. Only the second sink
    // differs. The callsite recorder tells the module host which
    // modules registered tracing callsites of their own (module.rs).
    let reg = tracing_subscriber::registry()
        .with(filter)
        .with(RingLayer)
        .with(crate::module::CallsiteRecorder);
    // A failed install is said on stderr, the one channel left: with no
    // global subscriber every `tracing` macro is a no-op (the level hint
    // stays OFF), so a module's own callsites never register and the
    // host unloads what it should keep mapped — which is how the flake's
    // sandbox failed the irc-module case for days while this error was
    // dropped on the floor (2026-10-13).
    fn report(r: Result<(), tracing_subscriber::util::TryInitError>) {
        if let Err(e) = r {
            eprintln!("veles: the tracing subscriber was not installed: {e}");
        }
    }
    match mode {
        LogMode::Stdout => {
            report(reg.with(tracing_subscriber::fmt::layer()).try_init());
        }
        LogMode::Stderr => {
            report(
                reg.with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                    .try_init(),
            );
        }
        LogMode::Quiet => {
            let console = match std::env::var("RUST_LOG") {
                Ok(v) if !v.trim().is_empty() => filter_from(Some(v)),
                _ => tracing_subscriber::EnvFilter::new("warn"),
            };
            report(
                reg.with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_filter(console),
                )
                .try_init(),
            );
        }
        LogMode::Captured => match log_file() {
            // No stdout layer at all: ratatui owns that handle.
            Some(f) => {
                report(
                    reg.with(
                        tracing_subscriber::fmt::layer()
                            .with_ansi(false)
                            .with_writer(f),
                    )
                    .try_init(),
                );
            }
            None => {
                report(reg.try_init());
            }
        },
    }
    chosen
}

/// `VELES_HOME/logs/veles.log`, appended. None when it can't be opened —
/// a missing log file must never stop the UI from starting.
fn log_file() -> Option<std::fs::File> {
    let dir = crate::config::veles_home().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("veles.log");
    // Roll at 8 MB — one file, no rotation library, no unbounded growth.
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 8 * 1024 * 1024 {
            let _ = std::fs::rename(&path, dir.join("veles.log.1"));
        }
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

/// Where the log file lives (shown in the TUI's log pane header).
pub fn log_path() -> std::path::PathBuf {
    crate::config::veles_home().join("logs").join("veles.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring is process-global and cargo runs tests in parallel, so
    /// they must not interleave pushes (pitfall 44 — the same trap the
    /// checkpoint tests hit).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn ring_evicts_oldest_and_keeps_order() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        for i in 0..(CAP + 25) {
            push(LogLine {
                time: "00:00:00".into(),
                level: "INFO".into(),
                target: "t".into(),
                msg: format!("line {i}"),
            });
        }
        assert_eq!(len(), CAP, "ring is bounded");
        let snap = snapshot(CAP);
        assert_eq!(snap.len(), CAP);
        // Oldest survivor is line 25; newest is the last pushed.
        assert_eq!(snap.first().unwrap().msg, "line 25");
        assert_eq!(snap.last().unwrap().msg, format!("line {}", CAP + 24));
        clear();
    }

    #[test]
    fn snapshot_limit_takes_the_tail() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        for i in 0..10 {
            push(LogLine {
                time: "00:00:00".into(),
                level: "WARN".into(),
                target: "t".into(),
                msg: format!("{i}"),
            });
        }
        let snap = snapshot(3);
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].msg, "7");
        assert_eq!(snap[2].msg, "9");
        clear();
    }

    #[test]
    fn init_is_idempotent() {
        // Whatever ran first wins; a second call must not panic.
        let a = init(LogMode::Captured);
        let b = init(LogMode::Stdout);
        assert_eq!(a, b, "the first mode sticks");
    }

    /// `RUST_LOG=` (nixpkgs' rust builder exports exactly that) used to
    /// turn every log line and every callsite off; it is "info" now,
    /// like unset. `off` stays off and a real directive is honoured.
    #[test]
    fn an_empty_rust_log_is_unset_not_off() {
        use tracing::level_filters::LevelFilter;
        let hint = |v: Option<&str>| filter_from(v.map(str::to_string)).max_level_hint();
        assert_eq!(hint(None), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("")), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("  ")), Some(LevelFilter::INFO));
        assert_eq!(hint(Some("off")), Some(LevelFilter::OFF));
        assert_eq!(hint(Some("debug")), Some(LevelFilter::DEBUG));
        assert_eq!(hint(Some("veles_core=trace")), Some(LevelFilter::TRACE));
    }

    #[test]
    fn an_event_inherits_its_enclosing_spans_fields() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        // One layer only, so the test drives RingLayer directly regardless
        // of what a global init already installed (init is once-per-process).
        use tracing_subscriber::prelude::*;
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(RingLayer));
        tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!("irc", network = "synirc", presence = "bot");
            let _e = span.enter();
            tracing::info!("connecting");
        });
        let snap = snapshot(4);
        let line = snap.iter().find(|l| l.msg.contains("connecting"));
        assert!(line.is_some(), "the event landed in the ring: {snap:?}");
        let line = line.unwrap();
        assert!(
            line.msg.contains("network=synirc") && line.msg.contains("presence=bot"),
            "the line names its enclosing span's fields, got: {}",
            line.msg
        );
        // An event outside the span carries no such fields.
        clear();
        tracing::dispatcher::with_default(&dispatch, || tracing::info!("bare"));
        let bare = snapshot(1);
        assert!(
            !bare[0].msg.contains("network="),
            "no span, no span fields: {}",
            bare[0].msg
        );
        clear();
    }

    #[test]
    fn two_networks_lines_each_name_their_own() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        use tracing_subscriber::prelude::*;
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(RingLayer));
        // The shape run_presence_loop builds: one span per connection,
        // interleaved lines. Each must name ITS network, not a sibling's.
        tracing::dispatcher::with_default(&dispatch, || {
            let a = tracing::info_span!("irc", network = "synirc", presence = "bot");
            let b = tracing::info_span!("irc", network = "irc4fun", presence = "bot");
            a.in_scope(|| tracing::info!("connecting to irc.synirc.net"));
            b.in_scope(|| tracing::info!("connecting to irc.irc4fun.net"));
            a.in_scope(|| tracing::info!("connection ended cleanly"));
        });
        let snap = snapshot(8);
        let syn: Vec<_> = snap
            .iter()
            .filter(|l| l.msg.contains("synirc.net"))
            .collect();
        let fun: Vec<_> = snap
            .iter()
            .filter(|l| l.msg.contains("irc4fun.net"))
            .collect();
        assert_eq!(syn.len(), 1, "{snap:?}");
        assert_eq!(fun.len(), 1, "{snap:?}");
        assert!(syn[0].msg.contains("network=synirc"), "{}", syn[0].msg);
        assert!(!syn[0].msg.contains("network=irc4fun"), "{}", syn[0].msg);
        assert!(fun[0].msg.contains("network=irc4fun"), "{}", fun[0].msg);
        assert!(!fun[0].msg.contains("network=synirc"), "{}", fun[0].msg);
        let ended = snap
            .iter()
            .find(|l| l.msg.contains("ended cleanly"))
            .unwrap();
        assert!(ended.msg.contains("network=synirc"), "{}", ended.msg);
        clear();
    }
}
