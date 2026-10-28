//! Layer slots (PLAN-plugins-installer.md M4 / R7): the open core.
//!
//! Four core subsystems — the turn runner, the system prompt builder,
//! tool dispatch and inbound routing — are owner-tracked STACKS a module
//! (or a script host) can replace or wrap, "like the python harnesses
//! where everything can be overridden". A layer gets the input and a
//! `next`: calling `next` runs what sits below it (the install before
//! it, core's own implementation at the bottom), not calling it replaces
//! the subsystem outright. Installs stack in order, the latest on top;
//! one install per owner per slot (a second by the same owner replaces
//! its first, the [`OwnedSlot`] rule). A purge — part of a module's
//! teardown — removes exactly that owner's layer, so what it covered is
//! in effect again: unloading restores the previous implementation.
//!
//! The owner is read from the installing thread's tag, never passed in,
//! the same as every other registry (`owned.rs`): a module installing
//! from its instance thread is tracked without saying who it is, and a
//! core install (`None`) is never purged.
//!
//! The common case has NO layer installed, and that path costs a lock
//! and a length check: every seam asks [`turn_layers`] (etc.) first and
//! calls its own code directly when the stack is empty, so the gateway
//! without modules boxes nothing it did not box before.
//!
//! The turn runner's seam deserves a word. `Agent::run_turn` is generic
//! over the caller's event closure, which need not be `Send` (the REPL's
//! prints), while a layer is a trait object whose future must be `Send`
//! to cross the chain. So the layered path hands the layers a
//! [`TurnEvents`] sender and the caller's closure stays where it is:
//! `run_turn` drains the channel into it between polls of the chain
//! ([`DrainInto`]), on the caller's thread, in order. A layer that emits
//! nothing and calls `next` changes no event a caller sees.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use serde_json::Value;

use crate::acl::Principal;
use crate::agent::{Agent, TurnResult};
use crate::config::VelesConfig;
use crate::owned::OwnedSlot;
use crate::providers::{CancelToken, ChatMessage, StreamEvent};
use crate::skills::SkillSelection;
use crate::tools::ToolRegistry;
use veles_types::bus::InboundMessage;

/// A boxed `Send` future, what every async layer returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

// ── the four stacks ──────────────────────────────────────────────────

static TURN: OwnedSlot<Arc<dyn TurnLayer>> = OwnedSlot::new();
static PROMPT: OwnedSlot<Arc<dyn PromptLayer>> = OwnedSlot::new();
static TOOL: OwnedSlot<Arc<dyn ToolLayer>> = OwnedSlot::new();
static INBOUND: OwnedSlot<Arc<dyn InboundLayer>> = OwnedSlot::new();

/// Remove `owner`'s layer from every slot; the count removed. The
/// module host calls this in its teardown (`purge_owner_everywhere`).
pub fn purge_owner(owner: Option<&'static str>) -> usize {
    [
        TURN.purge(owner),
        PROMPT.purge(owner),
        TOOL.purge(owner),
        INBOUND.purge(owner),
    ]
    .into_iter()
    .filter(|b| *b)
    .count()
}

/// The owner of the layer on top of each slot, for `/plugins` and the
/// tests: `(slot, owner)`, only the slots with a layer installed.
pub fn installed() -> Vec<(&'static str, Option<&'static str>)> {
    let mut out = Vec::new();
    if let Some(o) = TURN.owner() {
        out.push(("turn", o));
    }
    if let Some(o) = PROMPT.owner() {
        out.push(("prompt", o));
    }
    if let Some(o) = TOOL.owner() {
        out.push(("tool", o));
    }
    if let Some(o) = INBOUND.owner() {
        out.push(("inbound", o));
    }
    out
}

// ── the turn runner ──────────────────────────────────────────────────

/// What a turn layer is handed: the agent, the history the turn appends
/// to, the prompt, the cancel token and the event sink.
pub struct TurnCx<'a> {
    pub agent: &'a Agent,
    pub history: &'a mut Vec<ChatMessage>,
    pub user_text: &'a str,
    pub cancel: Option<CancelToken>,
    pub events: TurnEvents,
}

/// The stream-event sink a layered turn writes to. Clone it into
/// whatever the layer spawns; the events reach the caller's closure in
/// the order they were sent.
#[derive(Clone)]
pub struct TurnEvents(std::sync::mpsc::Sender<StreamEvent>);

impl TurnEvents {
    /// A sender and its receiver; `run_turn` drains the receiver.
    pub fn channel() -> (Self, std::sync::mpsc::Receiver<StreamEvent>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self(tx), rx)
    }

    pub fn emit(&self, ev: StreamEvent) {
        let _ = self.0.send(ev);
    }
}

/// A layer over the turn runner. `next.run(cx)` runs the layer below
/// (core's `run_turn_inner` at the bottom).
pub trait TurnLayer: Send + Sync {
    fn run<'a>(
        &'a self,
        cx: TurnCx<'a>,
        next: NextTurn<'a>,
    ) -> BoxFuture<'a, anyhow::Result<TurnResult>>;
}

/// The bottom of the turn chain: core's own runner, as the seam hands
/// it in.
pub type TurnBase<'a> =
    dyn Fn(TurnCx<'a>) -> BoxFuture<'a, anyhow::Result<TurnResult>> + Send + Sync + 'a;

/// The rest of the chain below a turn layer.
pub struct NextTurn<'a> {
    rest: &'a [Arc<dyn TurnLayer>],
    base: &'a TurnBase<'a>,
}

impl<'a> NextTurn<'a> {
    pub fn run(self, cx: TurnCx<'a>) -> BoxFuture<'a, anyhow::Result<TurnResult>> {
        match self.rest.split_last() {
            Some((top, rest)) => top.run(
                cx,
                NextTurn {
                    rest,
                    base: self.base,
                },
            ),
            None => (self.base)(cx),
        }
    }
}

/// Install a turn layer for the current thread's owner.
pub fn install_turn(layer: Arc<dyn TurnLayer>) {
    TURN.set(layer);
}

/// The turn stack, bottom to top. Empty for the direct path.
pub fn turn_layers() -> Vec<Arc<dyn TurnLayer>> {
    TURN.all()
}

/// Run `cx` through `layers` (top first) down to `base`.
pub fn run_turn_layers<'a>(
    layers: &'a [Arc<dyn TurnLayer>],
    cx: TurnCx<'a>,
    base: &'a TurnBase<'a>,
) -> BoxFuture<'a, anyhow::Result<TurnResult>> {
    NextTurn { rest: layers, base }.run(cx)
}

/// Polls `fut` and, around each poll, hands every event the layers sent
/// so far to `on_event` — on this thread, so the caller's closure needs
/// no `Send`. The final drain after completion delivers what the last
/// poll produced.
pub struct DrainInto<'f, Fut, F> {
    fut: Fut,
    rx: std::sync::mpsc::Receiver<StreamEvent>,
    on_event: &'f mut F,
}

impl<'f, Fut, F> DrainInto<'f, Fut, F> {
    pub fn new(fut: Fut, rx: std::sync::mpsc::Receiver<StreamEvent>, on_event: &'f mut F) -> Self {
        Self { fut, rx, on_event }
    }
}

impl<Fut, F> Future for DrainInto<'_, Fut, F>
where
    Fut: Future + Unpin,
    F: FnMut(StreamEvent),
{
    type Output = Fut::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let r = Pin::new(&mut this.fut).poll(cx);
        while let Ok(ev) = this.rx.try_recv() {
            (this.on_event)(ev);
        }
        r
    }
}

// ── the system prompt builder ────────────────────────────────────────

/// What a prompt layer is handed: the config, who the prompt is for
/// (`None` = an owner surface) and the session's skill selection.
pub struct PromptCx<'a> {
    pub cfg: &'a VelesConfig,
    pub principal: Option<&'a Principal>,
    pub skills: &'a SkillSelection,
}

/// A layer over the system prompt builder (sync: the prompt is built
/// once per session). `next.build(cx)` is the prompt the layers below
/// produce; return it changed, or something else entirely.
pub trait PromptLayer: Send + Sync {
    fn build(&self, cx: &PromptCx<'_>, next: NextPrompt<'_>) -> String;
}

/// The bottom of the prompt chain.
pub type PromptBase = dyn Fn(&PromptCx<'_>) -> String + Send + Sync;

pub struct NextPrompt<'a> {
    rest: &'a [Arc<dyn PromptLayer>],
    base: &'a PromptBase,
}

impl NextPrompt<'_> {
    pub fn build(self, cx: &PromptCx<'_>) -> String {
        match self.rest.split_last() {
            Some((top, rest)) => top.build(
                cx,
                NextPrompt {
                    rest,
                    base: self.base,
                },
            ),
            None => (self.base)(cx),
        }
    }
}

pub fn install_prompt(layer: Arc<dyn PromptLayer>) {
    PROMPT.set(layer);
}

pub fn prompt_layers() -> Vec<Arc<dyn PromptLayer>> {
    PROMPT.all()
}

pub fn run_prompt_layers(
    layers: &[Arc<dyn PromptLayer>],
    cx: &PromptCx<'_>,
    base: &PromptBase,
) -> String {
    NextPrompt { rest: layers, base }.build(cx)
}

// ── tool dispatch ────────────────────────────────────────────────────

/// What a tool layer is handed: the session's registry, the name the
/// caller used (an alias is resolved below) and the arguments.
pub struct ToolCx<'a> {
    pub registry: &'a ToolRegistry,
    pub name: String,
    pub args: Value,
}

/// A layer over tool dispatch. `next.call(cx)` runs the layer below
/// (core's gated dispatch at the bottom); the `String` is the tool
/// result the model sees.
pub trait ToolLayer: Send + Sync {
    fn call<'a>(&'a self, cx: ToolCx<'a>, next: NextTool<'a>) -> BoxFuture<'a, String>;
}

pub type ToolBase<'a> = dyn Fn(ToolCx<'a>) -> BoxFuture<'a, String> + Send + Sync + 'a;

pub struct NextTool<'a> {
    rest: &'a [Arc<dyn ToolLayer>],
    base: &'a ToolBase<'a>,
}

impl<'a> NextTool<'a> {
    pub fn call(self, cx: ToolCx<'a>) -> BoxFuture<'a, String> {
        match self.rest.split_last() {
            Some((top, rest)) => top.call(
                cx,
                NextTool {
                    rest,
                    base: self.base,
                },
            ),
            None => (self.base)(cx),
        }
    }
}

pub fn install_tool(layer: Arc<dyn ToolLayer>) {
    TOOL.set(layer);
}

pub fn tool_layers() -> Vec<Arc<dyn ToolLayer>> {
    TOOL.all()
}

pub fn run_tool_layers<'a>(
    layers: &'a [Arc<dyn ToolLayer>],
    cx: ToolCx<'a>,
    base: &'a ToolBase<'a>,
) -> BoxFuture<'a, String> {
    NextTool { rest: layers, base }.call(cx)
}

// ── inbound routing ──────────────────────────────────────────────────

/// A layer over inbound routing: every message a platform delivers to
/// the gateway passes here before the ACL, the commands and the turn.
/// `next.handle(msg)` routes it as core would; dropping it without
/// calling `next` discards it; calling `next` with another message
/// rewrites it.
pub trait InboundLayer: Send + Sync {
    fn handle<'a>(&'a self, msg: InboundMessage, next: NextInbound<'a>) -> BoxFuture<'a, ()>;
}

/// The bottom of the inbound chain. Owned (an `Arc`) rather than
/// borrowed like the other bases: the gateway's base is a closure over
/// `&GatewayCore` whose type carries the chain's lifetime, and a borrow
/// of such a local trips dropck; the fn-item bases above carry nothing.
pub type InboundBase<'a> = dyn Fn(InboundMessage) -> BoxFuture<'a, ()> + Send + Sync + 'a;

pub struct NextInbound<'a> {
    rest: &'a [Arc<dyn InboundLayer>],
    base: Arc<InboundBase<'a>>,
}

impl<'a> NextInbound<'a> {
    pub fn handle(self, msg: InboundMessage) -> BoxFuture<'a, ()> {
        match self.rest.split_last() {
            Some((top, rest)) => top.handle(
                msg,
                NextInbound {
                    rest,
                    base: self.base,
                },
            ),
            None => (self.base)(msg),
        }
    }
}

pub fn install_inbound(layer: Arc<dyn InboundLayer>) {
    INBOUND.set(layer);
}

pub fn inbound_layers() -> Vec<Arc<dyn InboundLayer>> {
    INBOUND.all()
}

pub fn run_inbound_layers<'a>(
    layers: &'a [Arc<dyn InboundLayer>],
    msg: InboundMessage,
    base: Arc<InboundBase<'a>>,
) -> BoxFuture<'a, ()> {
    NextInbound { rest: layers, base }.handle(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    /// The slots are process-global; the tests that install into them
    /// run one at a time. A tokio mutex, so the async test may hold it
    /// across its awaits (a std guard there is `await_holding_lock`).
    static SERIAL: Mutex<()> = Mutex::const_new(());

    fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        SERIAL.blocking_lock()
    }

    /// Run `f` with the thread tagged `owner`, restoring the tag after.
    fn as_owner<R>(owner: &'static str, f: impl FnOnce() -> R) -> R {
        let prev = crate::lifecycle::thread_owner();
        crate::lifecycle::set_thread_owner(owner);
        let r = f();
        crate::lifecycle::clear_thread_owner_for_test(prev);
        r
    }

    struct Append(&'static str);
    impl PromptLayer for Append {
        fn build(&self, cx: &PromptCx<'_>, next: NextPrompt<'_>) -> String {
            format!("{}+{}", next.build(cx), self.0)
        }
    }

    struct Replace(&'static str);
    impl PromptLayer for Replace {
        fn build(&self, _cx: &PromptCx<'_>, _next: NextPrompt<'_>) -> String {
            self.0.to_string()
        }
    }

    fn prompt_through(cfg: &VelesConfig) -> String {
        let cx = PromptCx {
            cfg,
            principal: None,
            skills: &SkillSelection::default(),
        };
        let base: &PromptBase = &|_cx: &PromptCx<'_>| "base".to_string();
        run_prompt_layers(&prompt_layers(), &cx, base)
    }

    /// Wrapping calls through to the layer below; replacing does not;
    /// the latest install is on top; a purge of one owner leaves the
    /// other's layer in effect and the base below it.
    #[test]
    fn layers_stack_wrap_replace_and_purge_in_owner_order() {
        let _s = serial();
        let cfg = VelesConfig::default();
        assert!(prompt_layers().is_empty(), "a clean slot to start");
        assert_eq!(prompt_through(&cfg), "base");

        as_owner("module:a", || install_prompt(Arc::new(Append("a"))));
        assert_eq!(prompt_through(&cfg), "base+a");

        as_owner("module:b", || install_prompt(Arc::new(Append("b"))));
        assert_eq!(
            prompt_through(&cfg),
            "base+a+b",
            "b installed later sits on top"
        );

        // b replaces its own layer: still one per owner, still on top.
        as_owner("module:b", || install_prompt(Arc::new(Replace("only-b"))));
        assert_eq!(
            prompt_through(&cfg),
            "only-b",
            "a replacing layer never reaches a or the base"
        );
        assert_eq!(
            installed(),
            vec![("prompt", Some("module:b"))],
            "the top owner is what the listing names"
        );

        assert_eq!(purge_owner(Some("module:b")), 1);
        assert_eq!(
            prompt_through(&cfg),
            "base+a",
            "unloading b restores a over the base"
        );
        assert_eq!(purge_owner(Some("module:b")), 0, "nothing left of b");
        assert_eq!(purge_owner(Some("module:a")), 1);
        assert_eq!(prompt_through(&cfg), "base");
        assert!(installed().is_empty());
    }

    /// A core install (no thread tag) is never purged, and a module's
    /// layer stacks over it.
    #[test]
    fn a_core_layer_survives_every_purge() {
        let _s = serial();
        let cfg = VelesConfig::default();
        install_prompt(Arc::new(Append("core")));
        as_owner("module:m", || install_prompt(Arc::new(Append("m"))));
        assert_eq!(prompt_through(&cfg), "base+core+m");
        assert_eq!(purge_owner(Some("module:m")), 1);
        assert_eq!(purge_owner(None), 0, "None is core: nothing to purge");
        assert_eq!(prompt_through(&cfg), "base+core");
        PROMPT.clear(); // the test's own core install, removed by its owner
        assert!(prompt_layers().is_empty());
    }

    struct Tag(&'static str);
    impl ToolLayer for Tag {
        fn call<'a>(&'a self, cx: ToolCx<'a>, next: NextTool<'a>) -> BoxFuture<'a, String> {
            Box::pin(async move {
                let inner = next.call(cx).await;
                format!("[{}]{inner}", self.0)
            })
        }
    }

    struct Swallow;
    impl ToolLayer for Swallow {
        fn call<'a>(&'a self, cx: ToolCx<'a>, _next: NextTool<'a>) -> BoxFuture<'a, String> {
            Box::pin(async move { format!("swallowed {}", cx.name) })
        }
    }

    /// The async chain has the same shape: the top layer runs first,
    /// `next` reaches the base, a replacing layer stops the chain.
    #[tokio::test]
    async fn an_async_chain_runs_top_down_to_the_base() {
        let _s = SERIAL.lock().await;
        let registry = ToolRegistry::default();
        let base: &ToolBase<'_> =
            &|cx: ToolCx<'_>| Box::pin(async move { format!("base({}:{})", cx.name, cx.args) });
        let cx = || ToolCx {
            registry: &registry,
            name: "t".into(),
            args: Value::Null,
        };
        let layers = tool_layers();
        assert_eq!(run_tool_layers(&layers, cx(), base).await, "base(t:null)");

        as_owner("module:x", || install_tool(Arc::new(Tag("x"))));
        as_owner("module:y", || install_tool(Arc::new(Tag("y"))));
        let layers = tool_layers();
        assert_eq!(
            run_tool_layers(&layers, cx(), base).await,
            "[y][x]base(t:null)",
            "y (on top) wraps x wraps the base"
        );
        as_owner("module:z", || install_tool(Arc::new(Swallow)));
        let layers = tool_layers();
        assert_eq!(run_tool_layers(&layers, cx(), base).await, "swallowed t");
        assert_eq!(purge_owner(Some("module:z")), 1);
        let layers = tool_layers();
        assert_eq!(
            run_tool_layers(&layers, cx(), base).await,
            "[y][x]base(t:null)"
        );
        purge_owner(Some("module:x"));
        purge_owner(Some("module:y"));
        assert!(tool_layers().is_empty());
    }

    /// Events a layer emits reach the caller's closure, in order, on
    /// the caller's thread — the closure needs no `Send`.
    #[tokio::test]
    async fn drain_into_hands_layer_events_to_the_callers_closure() {
        let (events, rx) = TurnEvents::channel();
        let fut: BoxFuture<'_, u8> = Box::pin(async move {
            events.emit(StreamEvent::Delta("one".into()));
            tokio::task::yield_now().await;
            events.emit(StreamEvent::Delta("two".into()));
            7
        });
        // A non-Send sink: an Rc proves the closure stays on this thread.
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sink = seen.clone();
        let mut on_event = move |ev: StreamEvent| {
            if let StreamEvent::Delta(s) = ev {
                sink.borrow_mut().push(s);
            }
        };
        let out = DrainInto::new(fut, rx, &mut on_event).await;
        assert_eq!(out, 7);
        assert_eq!(*seen.borrow(), vec!["one".to_string(), "two".to_string()]);
    }
}
