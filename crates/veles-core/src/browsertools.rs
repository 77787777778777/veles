//! Browser automation tools — agent-browser CLI bridge.
//!
//! #26 in the feature matrix. Wraps the `agent-browser` CLI (the same
//! npm package hermes uses; nixpkgs/system install here) in eight
//! tools: navigate (`browser_navigate`), snapshot, click, type,
//! scroll, press, back, console. Each call runs agent-browser in
//! ONE-SHOT CLI mode with `--json`; the daemon behind the CLI keeps
//! the browser (and its page) alive between calls, so refs from a
//! snapshot carry over to the next click/type.
//!
//! This bridge is also the local, free replacement for z.ai's MCP
//! webReader: `browser_navigate` + `agent-browser get text` render
//! JavaScript-heavy pages through the real Chrome, no key. For plain
//! static pages `web_extract` stays cheaper.

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use crate::tools::Tool;

pub struct BrowserTool;

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Open a URL in the browser automation session. Returns a text-based snapshot of the page (accessibility tree with interactive element refs)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "URL to navigate to"}
            },
            "required": ["url"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let url = args["url"].as_str().unwrap_or_default().to_string();
        if url.is_empty() {
            return "error: url is required".into();
        }
        // Someone else's page opens in their box, whose own fence keeps
        // it off private addresses; it is still refused here first, as
        // the guest browser's is, so file:// and the rest say why.
        if caller() == Ok(Session::Guest) {
            if let Err(why) = crate::urlguard::check(&url).await {
                return format!("error: {why}");
            }
        }
        // Open, then fold in the interactive snapshot so one call
        // returns what the page IS, mirroring the old contract.
        let opened = browser_action(&["open", &url], NAV_TIMEOUT).await;
        if opened.starts_with("error:") {
            return opened;
        }
        let snap = browser_argv(&snapshot_argv(false), NAV_TIMEOUT).await;
        with_challenge(format!("{opened}\n\n{snap}")).await
    }
}

pub struct BrowserSnapshotTool;

#[async_trait]
impl Tool for BrowserSnapshotTool {
    fn name(&self) -> &str {
        "browser_snapshot"
    }
    fn description(&self) -> &str {
        "Get a text-based snapshot of the current page's accessibility tree. Returns interactive elements with ref IDs (like @e5, @e12) for use with browser_click."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "full": {
                    "type": "boolean",
                    "description": "If true, returns complete page content. If false (default), returns compact interactive elements only."
                }
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        let full = args["full"].as_bool().unwrap_or(false);
        with_challenge(browser_argv(&snapshot_argv(full), NAV_TIMEOUT).await).await
    }
}

pub struct BrowserClickTool;

#[async_trait]
impl Tool for BrowserClickTool {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click on an element identified by its ref ID from the snapshot (e.g. '@e5')."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Element reference from snapshot (e.g. '@e5')"}
            },
            "required": ["ref"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(ref_id) = args["ref"].as_str() else {
            return "error: ref is required".into();
        };
        with_challenge(browser_action(&["click", ref_id], CLICK_TIMEOUT).await).await
    }
}

pub struct BrowserTypeTool;

#[async_trait]
impl Tool for BrowserTypeTool {
    fn name(&self) -> &str {
        "browser_type"
    }
    fn description(&self) -> &str {
        "Type text into an input field identified by its ref ID from the snapshot."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Element reference (e.g. '@e3')"},
                "text": {"type": "string", "description": "Text to type into the field"}
            },
            "required": ["ref", "text"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(ref_id) = args["ref"].as_str() else {
            return "error: ref is required".into();
        };
        let Some(text) = args["text"].as_str() else {
            return "error: text is required".into();
        };
        // fill = clear-then-type. The old bridge's "type" appended into
        // whatever the field already held; for agent use, replacing is
        // the sane default (matches hermes' browser_type behavior).
        browser_action(&["fill", ref_id, text], CLICK_TIMEOUT).await
    }
}

pub struct BrowserScrollTool;

#[async_trait]
impl Tool for BrowserScrollTool {
    fn name(&self) -> &str {
        "browser_scroll"
    }
    fn description(&self) -> &str {
        "Scroll the page up or down to reveal more content."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "direction": {
                    "type": "string",
                    "enum": ["up", "down"],
                    "description": "Direction to scroll"
                },
                "amount": {
                    "type": "integer",
                    "description": "Pixels to scroll (default one viewport)"
                }
            },
            "required": ["direction"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let direction = args["direction"].as_str().unwrap_or("down");
        match args["amount"].as_u64() {
            Some(px) => {
                let px = px.to_string();
                browser_action(&["scroll", direction, &px], CLICK_TIMEOUT).await
            }
            None => browser_action(&["scroll", direction], CLICK_TIMEOUT).await,
        }
    }
}

pub struct BrowserPressTool;

#[async_trait]
impl Tool for BrowserPressTool {
    fn name(&self) -> &str {
        "browser_press"
    }
    fn description(&self) -> &str {
        "Press a keyboard key (e.g. Enter, Tab, Escape)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "Key to press (e.g. 'Enter', 'Tab', 'Escape')"}
            },
            "required": ["key"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let Some(key) = args["key"].as_str() else {
            return "error: key is required".into();
        };
        with_challenge(browser_action(&["press", key], CLICK_TIMEOUT).await).await
    }
}

pub struct BrowserBackTool;

#[async_trait]
impl Tool for BrowserBackTool {
    fn name(&self) -> &str {
        "browser_back"
    }
    fn description(&self) -> &str {
        "Navigate back to the previous page in browser history."
    }
    fn schema(&self) -> Value {
        serde_json::json!({"type": "object"})
    }
    async fn call(&self, _args: Value) -> String {
        with_challenge(browser_action(&["back"], NAV_TIMEOUT).await).await
    }
}

pub struct BrowserConsoleTool;

#[async_trait]
impl Tool for BrowserConsoleTool {
    fn name(&self) -> &str {
        "browser_console"
    }
    fn description(&self) -> &str {
        "Get browser console output and JavaScript errors. Can also evaluate a JavaScript expression in the page context."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "expression": {
                    "type": "string",
                    "description": "Optional JavaScript expression to evaluate in the page context"
                }
            }
        })
    }
    async fn call(&self, args: Value) -> String {
        match args["expression"].as_str() {
            Some(expr) => browser_action(&["eval", expr], CLICK_TIMEOUT).await,
            None => browser_action(&["console"], CLICK_TIMEOUT).await,
        }
    }
}

// ── bot challenges (PLAN-pool-round.md Part E) ───────────────────────

/// What a page is, in one eval: enough for `challenge::in_page`.
const PAGE_STATE_JS: &str = "JSON.stringify({url: location.href, title: document.title, \
    frames: Array.from(document.querySelectorAll('iframe')).map(f => f.src || ''), \
    text: (document.body ? document.body.innerText : '').slice(0, 4000)})";

struct PageState {
    url: String,
    title: String,
    frames: Vec<String>,
    text: String,
}

async fn page_state() -> Option<PageState> {
    page_state_in(caller().ok()?).await
}

async fn page_state_in(s: Session) -> Option<PageState> {
    let out = browser_action_in(s, &["eval", PAGE_STATE_JS], CLICK_TIMEOUT).await;
    let data: Value = serde_json::from_str(&out).ok()?;
    let v: Value = match &data["result"] {
        Value::String(s) => serde_json::from_str(s).ok()?,
        other => other.clone(),
    };
    Some(PageState {
        url: v["url"].as_str().unwrap_or_default().to_string(),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        frames: v["frames"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        text: v["text"].as_str().unwrap_or_default().to_string(),
    })
}

async fn page_challenge_in(s: Session) -> Option<(crate::challenge::Challenge, String)> {
    let p = page_state_in(s).await?;
    crate::challenge::in_page(&p.url, &p.title, &p.text, &p.frames).map(|c| (c, p.url))
}

/// A browser result for a page that is behind a challenge says so on its
/// FIRST line — which is where the session's node looks (`tools.rs`
/// dispatch → `challenge::relay`), and which a page cannot write to.
///
/// A check that clears by itself is not one: the page's own result, as
/// it is once the check has gone, is the answer ([`standing_challenge_in`]).
async fn with_challenge(out: String) -> String {
    let Ok(s) = caller() else {
        return out;
    };
    if out.starts_with("error") || page_challenge_in(s).await.is_none() {
        return out;
    }
    match standing_challenge_in(s).await {
        Some((c, url)) => format!("{}\n{out}", crate::challenge::marker_line(&c, &url)),
        None => browser_argv_in(s, &snapshot_argv(false), NAV_TIMEOUT).await,
    }
}

/// How long a page behind a check gets to clear it on its own before a
/// person is asked. A drawn browser passed Cloudflare's managed check at
/// Coventry's Pure portal by itself within 5 seconds (2026-10-12), but it
/// still showed "Just a moment…" when the navigation returned. Asking at
/// that moment pinged the USER about a check that was already passing.
const SETTLE_SECS: u64 = 15;

/// The check on `s`'s page that is still there after [`SETTLE_SECS`];
/// `None` when the page has none, or it cleared by itself.
async fn standing_challenge_in(s: Session) -> Option<(crate::challenge::Challenge, String)> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(SETTLE_SECS);
    loop {
        let found = page_challenge_in(s).await?;
        if std::time::Instant::now() >= deadline {
            return Some(found);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// Is there a screen to draw a window on? A unix process knows from
/// `DISPLAY`/`WAYLAND_DISPLAY`; a Windows process in the logged-on
/// user's session (where the gateway runs, PLAN-windows.md) always has
/// the desktop.
fn has_display() -> bool {
    if cfg!(windows) {
        return true;
    }
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
}

/// The page behind a challenge, put to a person: `look` (a screenshot and
/// what stands in the way), `click`/`type`/`key` on their behalf, and
/// `window`/`unwindow` to move to a real browser window and back.
pub struct BrowserChallengeTool;

#[async_trait]
impl Tool for BrowserChallengeTool {
    fn name(&self) -> &str {
        "browser_challenge"
    }
    fn description(&self) -> &str {
        "A bot challenge (captcha) on the browser's page, put to the user: look = a screenshot and what stands in the way; click/type/key act on the page for them; window opens a real browser window where this node has a display, unwindow goes back to the headless one; page = the page once it is through (a PDF is downloaded). veles runs this itself when a page is behind a challenge — do not call it on your own."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["look", "click", "type", "key", "window", "unwindow", "page"]
                },
                "x": {"type": "integer", "description": "click: viewport x, CSS pixels"},
                "y": {"type": "integer", "description": "click: viewport y, CSS pixels"},
                "text": {"type": "string", "description": "type: the text"},
                "key": {"type": "string", "description": "key: e.g. Enter, Tab"}
            },
            "required": ["action"]
        })
    }
    async fn call(&self, args: Value) -> String {
        let s = match caller() {
            Ok(s) => s,
            Err(e) => return e,
        };
        match args["action"].as_str().unwrap_or_default() {
            // A window opens on this node's own screen: never for a page
            // that is someone else's (see `challenge_look_in`).
            "window" | "unwindow" if s == Session::Guest => {
                "error: no window opens for a page in a sandbox box".into()
            }
            "window" => challenge_window(true).await,
            "unwindow" => challenge_window(false).await,
            _ => challenge_act(s, &args).await,
        }
    }
}

/// `look`, `click`, `type` and `key` on a challenge page in `s`, and
/// `page`: the page once the challenge is gone — a PDF downloaded
/// (downloads.rs), anything else as a snapshot.
async fn challenge_act(s: Session, args: &Value) -> String {
    match args["action"].as_str().unwrap_or_default() {
        "look" => challenge_look_in(s).await,
        "page" => match pdf_download_in(s).await {
            Some(got) => got,
            None => browser_argv_in(s, &snapshot_argv(false), NAV_TIMEOUT).await,
        },
        "click" => {
            let (Some(x), Some(y)) = (args["x"].as_i64(), args["y"].as_i64()) else {
                return "error: click needs x and y".into();
            };
            if !(0..=10_000).contains(&x) || !(0..=10_000).contains(&y) {
                return "error: click is off the page".into();
            }
            let (x, y) = (x.to_string(), y.to_string());
            for step in [
                vec!["mouse", "move", x.as_str(), y.as_str()],
                vec!["mouse", "down"],
                vec!["mouse", "up"],
            ] {
                let r = browser_action_in(s, &step, CLICK_TIMEOUT).await;
                if r.starts_with("error") {
                    return r;
                }
            }
            // What a click on a challenge does takes a moment to land
            // (a checkbox animates, the page reloads).
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            "ok".into()
        }
        "type" => match args["text"].as_str() {
            Some(t) if !t.is_empty() => {
                browser_action_in(s, &["keyboard", "type", t], CLICK_TIMEOUT).await
            }
            _ => "error: type needs text".into(),
        },
        "key" => match args["key"].as_str() {
            Some(k) if !k.is_empty() => browser_action_in(s, &["press", k], CLICK_TIMEOUT).await,
            _ => "error: key needs a key name".into(),
        },
        other => format!("error: unknown action {other:?}"),
    }
}

/// A screenshot of the page in `s` and what stands in the way. The guest
/// never offers a window: whoever holds the relay link is not someone a
/// window on the owner's screen should open for.
async fn challenge_look_in(s: Session) -> String {
    use base64::Engine as _;
    let bytes = match screenshot_in(s).await {
        Ok(b) => b,
        Err(e) => return e,
    };
    let page = page_state_in(s).await;
    let (url, title, found) = match &page {
        Some(p) => (
            p.url.clone(),
            p.title.clone(),
            crate::challenge::in_page(&p.url, &p.title, &p.text, &p.frames),
        ),
        None => (String::new(), String::new(), None),
    };
    serde_json::json!({
        "image": base64::engine::general_purpose::STANDARD.encode(bytes),
        "url": url,
        "title": title,
        "challenge": found.map(|c| serde_json::json!({ "vendor": c.vendor, "evidence": c.evidence })),
        "display": s == Session::Agent && has_display(),
    })
    .to_string()
}

/// A screenshot of the page in `s`, as PNG bytes. A guest browser in a
/// box ([`guest_box`]) writes it inside the box, and it comes out as
/// base64 text, the only thing an exec carries whole.
async fn screenshot_in(s: Session) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    if let (Session::Guest, Some(key)) = (s, guest_box()) {
        let path = format!("/tmp/veles-challenge-{nanos}.png");
        let shot = box_cli(&key, &["screenshot".to_string(), path.clone()], NAV_TIMEOUT).await;
        if shot.starts_with("error") {
            return Err(shot);
        }
        let argv = [
            "sh",
            "-c",
            "base64 -w0 \"$1\"; s=$?; rm -f \"$1\"; exit $s",
            "sh",
            path.as_str(),
        ]
        .map(str::to_string);
        let out = crate::user_envs::exec_argv(&key, &argv, CLICK_TIMEOUT)
            .await
            .map_err(|e| format!("error: the guest browser's box: {e}"))?;
        if out.exit_code != 0 {
            return Err(format!(
                "error: the screenshot did not land: {}",
                out.stderr.trim()
            ));
        }
        return base64::engine::general_purpose::STANDARD
            .decode(out.stdout.trim())
            .map_err(|e| format!("error: the screenshot came out unreadable: {e}"));
    }
    let path = std::env::temp_dir().join(format!(
        "veles-challenge-{}-{nanos}.png",
        std::process::id()
    ));
    let shot = browser_argv_in(
        s,
        &["screenshot".to_string(), path.display().to_string()],
        NAV_TIMEOUT,
    )
    .await;
    if shot.starts_with("error") {
        return Err(shot);
    }
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    bytes.map_err(|e| format!("error: the screenshot did not land: {e}"))
}

/// Move the page to a window on this node's own screen (`headed`) or
/// back to the hidden browser, carrying its cookies and storage across
/// through a state file that is deleted as soon as it has been read — it
/// holds the session's logins. The browser closes and relaunches for
/// either move: the new one takes its display from the call that starts
/// it (agent-browser's daemon exits on `close`; checked 2026-10-12).
async fn challenge_window(headed: bool) -> String {
    if headed && !has_display() {
        return "error: this node has no display to open a window on".into();
    }
    let Some(page) = page_state().await else {
        return "error: no page is open".into();
    };
    let dir = crate::config::data_dir().join("browser");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("error: {e}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let state = dir.join("challenge-state.json").display().to_string();
    let saved = browser_argv(&["state".into(), "save".into(), state.clone()], NAV_TIMEOUT).await;
    if saved.starts_with("error") {
        return saved;
    }
    let _ = browser_action(&["close"], CLICK_TIMEOUT).await;
    WINDOWED.store(headed, std::sync::atomic::Ordering::SeqCst);
    let argv: Vec<String> = vec!["--state".into(), state.clone(), "open".into(), page.url];
    let opened = browser_argv(&argv, NAV_TIMEOUT).await;
    let _ = std::fs::remove_file(&state);
    if opened.starts_with("error") {
        // No window after all: the next call starts the hidden browser.
        WINDOWED.store(false, std::sync::atomic::Ordering::SeqCst);
        opened
    } else {
        "ok".into()
    }
}

const NAV_TIMEOUT: u64 = 45;
const CLICK_TIMEOUT: u64 = 30;

// ── where the browsers draw ──────────────────────────────────────────

/// Where a browser draws.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Draw {
    /// Headed, on this process's private Xvfb display: what nobody sees,
    /// and what a page cannot tell from a person's browser by its user
    /// agent, client hints, screen or GPU ([`private_display`]). Only
    /// unix starts an Xvfb; elsewhere the desktop itself is the screen.
    #[cfg_attr(not(unix), allow(dead_code))]
    Private(String),
    /// Headed, on this node's own screen: a challenge's window
    /// ([`challenge_window`]).
    Real,
    /// Headless: no Xvfb here, or `[web] browser_headless`.
    Headless,
}

/// Whether the agent's browser is in a window on this node's screen.
static WINDOWED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Launch flags a drawn browser takes. X11, because the display is an
/// X server (Chrome picks Wayland on its own here and then fails to
/// start), and a window the size of a laptop's, not Chrome's default.
const DRAWN_ARGS: &[&str] = if cfg!(unix) {
    &["--ozone-platform=x11", "--window-size=1280,900"]
} else {
    &["--window-size=1280,900"]
};

/// WebGL on this node's GPU where it has a render node: headless Chrome
/// and a GPU-less Xvfb both answer "SwiftShader", a software renderer
/// that a person's browser does not have. EGL takes the integrated GPU
/// (Vulkan woke the discrete one, measured 2026-10-12).
fn gpu_args() -> &'static [&'static str] {
    if cfg!(windows) {
        // Chrome on Windows picks ANGLE over D3D11 by itself.
        return &[];
    }
    let render_node = std::fs::read_dir("/dev/dri")
        .map(|d| {
            d.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
        .unwrap_or(false);
    if render_node {
        &["--use-gl=angle", "--use-angle=gl-egl"]
    } else {
        &["--enable-unsafe-swiftshader"]
    }
}

async fn draw_for(s: Session) -> Draw {
    if s == Session::Agent && WINDOWED.load(std::sync::atomic::Ordering::SeqCst) {
        return Draw::Real;
    }
    if crate::config::live().web.browser_headless {
        return Draw::Headless;
    }
    // Boxed: the futures above this one (a cron job's turn → the tool
    // chain → a browser call) were already 128 deep, the compiler's
    // layout limit, and this chain took them past it. A box is a
    // pointer, so the layout stops here.
    #[cfg(unix)]
    {
        match Box::pin(private_display()).await {
            Some(d) => Draw::Private(d),
            None => Draw::Headless,
        }
    }
    // No Xvfb to start: the desktop session itself is the display
    // (headed, in a window the person can see), else headless.
    #[cfg(not(unix))]
    {
        if has_display() {
            Draw::Real
        } else {
            Draw::Headless
        }
    }
}

/// An Xvfb this process started, and the X connection that keeps it up.
#[cfg(unix)]
struct Xvfb {
    name: String,
    child: tokio::process::Child,
    _keeper: tokio::net::UnixStream,
}

#[cfg(unix)]
fn xvfb_slot() -> &'static tokio::sync::Mutex<Option<Xvfb>> {
    static X: std::sync::OnceLock<tokio::sync::Mutex<Option<Xvfb>>> = std::sync::OnceLock::new();
    X.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// This process's private X display (`:N`), started on first use; `None`
/// where Xvfb cannot be had. Xvfb runs with `-terminate`, so it exits
/// once its last client is gone. One of them is this process's own idle
/// connection ([`keep`]): the display lasts while this process does, and
/// while a browser drawn on it does. A gateway's lives as long as the
/// gateway, and a `veles tool` run leaves none behind once its browser
/// idles out. No `pre_exec` death signal: a closure in the child forks,
/// and fork with libtcl in the process hangs (sandbox.rs).
#[cfg(unix)]
async fn private_display() -> Option<String> {
    let mut slot = xvfb_slot().lock().await;
    if let Some(x) = slot.as_mut() {
        if matches!(x.child.try_wait(), Ok(None)) {
            return Some(x.name.clone());
        }
        *slot = None;
    }
    match start_xvfb().await {
        Ok(x) => {
            let name = x.name.clone();
            tracing::info!("browsers draw on a private display, {name}");
            *slot = Some(x);
            Some(name)
        }
        Err(e) => {
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
                tracing::warn!("browsers run headless: no private display ({e:#})");
            }
            None
        }
    }
}

#[cfg(unix)]
async fn start_xvfb() -> anyhow::Result<Xvfb> {
    use anyhow::Context as _;
    use tokio::io::AsyncBufReadExt as _;
    let bin = on_path("Xvfb").context("Xvfb is not on PATH")?;
    let mut child = Command::new(bin)
        .args(["-displayfd", "1", "-screen", "0", "1920x1080x24"])
        .args(["-nolisten", "tcp", "-terminate"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("Xvfb did not start")?;
    let out = child.stdout.take().context("Xvfb has no stdout")?;
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::io::BufReader::new(out).read_line(&mut line),
    )
    .await
    .context("Xvfb named no display within 10s")??;
    let n: u32 = line.trim().parse().context("Xvfb named no display")?;
    let keeper = keep(n).await?;
    Ok(Xvfb {
        name: format!(":{n}"),
        child,
        _keeper: keeper,
    })
}

/// Connect to display `n` as an X client and stay connected. The X11
/// connection setup: byte order `l`, protocol 11.0, no authorization;
/// the server's first reply byte is 1 when it accepts.
#[cfg(unix)]
async fn keep(n: u32) -> anyhow::Result<tokio::net::UnixStream> {
    use anyhow::Context as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let mut s = tokio::net::UnixStream::connect(format!("/tmp/.X11-unix/X{n}"))
        .await
        .with_context(|| format!("display :{n} has no socket"))?;
    s.write_all(&[b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .await?;
    let mut head = [0u8; 8];
    s.read_exact(&mut head).await?;
    anyhow::ensure!(head[0] == 1, "display :{n} refused a connection");
    Ok(s)
}

/// A program on PATH, by name.
#[cfg(unix)]
fn on_path(name: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Which agent-browser session a call drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Session {
    /// The agent's own browser: the `browser_*` tools, owner sessions.
    Agent,
    /// The guest browser ([`guest`]): pages opened for a session that is
    /// not the owner's, with every request through `guestproxy`.
    Guest,
}

/// The flags that pick `s`, and for the guest also launch it fenced.
/// agent-browser applies launch flags when the session's browser starts
/// and reuses it while they stay the same, so every call passes them.
///
/// `--proxy-bypass "<-loopback>"`: Chrome sends loopback around a proxy
/// unless told not to. Without it a guest page reached 127.0.0.1
/// directly (checked 2026-10-11 with a logging proxy). The WebRTC policy
/// keeps UDP, which a proxy cannot carry, from leaving at all.
///
/// The session name carries the proxy's port: a restarted gateway has a
/// new port, and a guest browser still pointed at the old one would fail
/// every request. The old session idles out on its own.
///
/// Both browsers launch WITHOUT Chrome's automation flag, and move the
/// pointer the way a hand does. A person solves a challenge through
/// them, and Cloudflare rejected every solve in a browser that says
/// `navigator.webdriver = true`: the USER clicked the checkbox through
/// the relay and the page only reloaded (2026-10-11, IRC session
/// 82c4da). The relay's clicks are that person's, so an instant pointer
/// jump on the checkbox is the other half of the same tell.
///
/// `[web] browser_webdriver = true` puts the flag back, for a site that
/// treats a declared automaton better.
///
/// Where a display is to be had they also launch HEADED ([`Draw`]), with
/// the same launch flags on the private display and in a window, so the
/// calls made while a window is open reuse it rather than relaunch.
async fn session_flags(s: Session, draw: &Draw) -> Result<Vec<String>, String> {
    flags_for(s, crate::config::live().web.browser_webdriver, draw).await
}

async fn flags_for(s: Session, webdriver: bool, draw: &Draw) -> Result<Vec<String>, String> {
    let not_automated = (!webdriver).then_some("--disable-blink-features=AutomationControlled");
    let mut flags: Vec<String> = Vec::new();
    let mut launch: Vec<&str> = not_automated.into_iter().collect();
    if !matches!(draw, Draw::Headless) {
        flags.push("--headed".into());
        launch.extend(DRAWN_ARGS);
        launch.extend(gpu_args());
    }
    match s {
        Session::Agent => {}
        Session::Guest => {
            let port = crate::guestproxy::port().await?;
            flags.extend([
                "--session".into(),
                format!("veles-guest-{port}"),
                "--proxy".into(),
                format!("http://127.0.0.1:{port}"),
                "--proxy-bypass".into(),
                "<-loopback>".into(),
            ]);
            launch.push("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
        }
    }
    if !launch.is_empty() {
        flags.extend(["--args".into(), launch.join(",")]);
    }
    flags.extend(["--input-mode".into(), "human".into()]);
    Ok(flags)
}

async fn browser_argv_in(s: Session, argv: &[String], timeout_secs: u64) -> String {
    if s == Session::Guest {
        if let Some(key) = guest_box() {
            return box_cli(&key, argv, timeout_secs).await;
        }
    }
    let draw = draw_for(s).await;
    let mut all = match session_flags(s, &draw).await {
        Ok(f) => f,
        Err(e) => return format!("error: {e}"),
    };
    all.extend_from_slice(argv);
    let display = match &draw {
        Draw::Private(d) => Some(d.as_str()),
        Draw::Real | Draw::Headless => None,
    };
    run_cli(&all, timeout_secs, display).await
}

async fn browser_action_in(s: Session, argv: &[&str], timeout_secs: u64) -> String {
    let owned: Vec<String> = argv.iter().map(|a| a.to_string()).collect();
    browser_argv_in(s, &owned, timeout_secs).await
}

/// Core dispatch: run agent-browser's real CLI and return its output.
///
/// The bridge speaks agent-browser's actual verb grammar
/// (`open <url>`, `snapshot`, `click <@ref>`, `fill <@ref> <text>`,
/// `scroll <dir> [px]`, `press <key>`, `back`, `console`, `eval <js>`)
/// with `--json` for a machine envelope
/// (`{"success":bool,"data":…,"error":…}`). It does NOT speak a
/// `--json '{"action":…}'` object — that shape is not agent-browser's
/// CLI protocol; every call used to die with `Unknown command` until
/// this was root-caused 2026-09-15 (agent-browser 0.27.0).
///
/// These two drive the caller's browser ([`caller`]): the agent's own
/// for the owner, someone else's in their box, with its launch flags.
async fn browser_action(argv: &[&str], timeout_secs: u64) -> String {
    match caller() {
        Ok(s) => browser_action_in(s, argv, timeout_secs).await,
        Err(e) => e,
    }
}

/// Same, taking owned args (so callers can build flag lists).
async fn browser_argv(argv: &[String], timeout_secs: u64) -> String {
    match caller() {
        Ok(s) => browser_argv_in(s, argv, timeout_secs).await,
        Err(e) => e,
    }
}

/// The browser a `browser_*` call drives (PLAN-challenge-relay.md D3).
/// The owner's: the agent's own, from the owner's address. Anyone
/// else's, and the owner's sent their way (`/egress tor`): the guest
/// browser in that session's own sandbox box, on a node that sends their
/// traffic through its proxy, so its only way out is the box's (Tor).
/// Nowhere else: on the laptop it would leave from the USER's address
/// and share the node's browser, and the USER gave other people the
/// browser only "once their browsing runs in their own sandbox
/// container" (D3, 2026-10-11).
fn caller() -> Result<Session, String> {
    if crate::egress::others().is_none() {
        return Ok(Session::Agent);
    }
    match guest_box() {
        Some(_) => Ok(Session::Guest),
        None => Err(crate::egress::browser_refusal()),
    }
}

/// Run agent-browser with exactly these arguments. `display`: the X
/// display a browser it launches draws on, in place of this process's
/// own (and never its Wayland one).
async fn run_cli(argv: &[String], timeout_secs: u64, display: Option<&str>) -> String {
    let browser_bin = match find_browser().await {
        Some(b) => b,
        None => return "error: agent-browser not found. Install it or add it to PATH.".into(),
    };
    let mut cmd = Command::new(&browser_bin);
    cmd.arg("--json")
        .args(argv)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(d) = display {
        cmd.env("DISPLAY", d).env_remove("WAYLAND_DISPLAY");
    }
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), cmd.output()).await;

    match result {
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            format_ab_output(&stdout, &stderr)
        }
        Ok(Err(e)) => format!("error launching agent-browser: {e}"),
        Err(_) => format!("error: timeout ({timeout_secs}s)"),
    }
}

/// Turn agent-browser's `--json` envelope into tool text. Success →
/// the `data` object as compact JSON (or "ok" when there is nothing to
/// report). Failure → `error: <msg>` so the model sees a clean failure
/// instead of a raw envelope. Non-envelope output passes through.
fn format_ab_output(stdout: &str, stderr: &str) -> String {
    let trimmed = stdout.trim();
    if let Ok(v) = serde_json::from_str::<Value>(trimmed) {
        if v.get("success").is_some() {
            return if v["success"].as_bool() == Some(true) {
                match v.get("data") {
                    Some(Value::Null) | None => "ok".into(),
                    Some(d) => serde_json::to_string(d).unwrap_or_else(|_| "ok".into()),
                }
            } else {
                format!(
                    "error: {}",
                    v["error"].as_str().unwrap_or("unknown agent-browser error")
                )
            };
        }
        return trimmed.to_string();
    }
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        "(no output)".into()
    }
}

/// The `full` snapshot knob → agent-browser flags. Compact (default)
/// restricts the tree to interactive elements (`-i`); full is
/// everything, for page-reading.
fn snapshot_argv(full: bool) -> Vec<String> {
    if full {
        vec!["snapshot".into()]
    } else {
        vec!["snapshot".into(), "-i".into()]
    }
}

/// The guest browser (PLAN-challenge-relay.md, steps 1–2): where a page
/// from a session that is not the owner's is opened so a person can
/// solve the challenge in front of it. An INTERNAL operation, in no tool
/// registry, so no model can name it. The session's node runs it through
/// `ToolRegistry::dispatch_internal`, homed with the `browser_*` tools,
/// and a peer's call reaches it through `remotetool::execute`.
pub const GUEST_OP: &str = "browser_guest";

/// The most page text one guest call returns.
const GUEST_TEXT_MAX: usize = 60_000;

/// Is `name` an internal operation rather than a tool?
pub fn is_internal(name: &str) -> bool {
    name == GUEST_OP
}

/// Run an internal operation on this node.
pub async fn internal(name: &str, args: Value) -> String {
    match name {
        GUEST_OP => guest(args).await,
        other => format!("error: there is no internal operation {other}"),
    }
}

/// `open {url}`: the page in the guest browser, as text, with the
/// challenge marker on the first line when one is in the way (the line
/// the session's node starts a relay on). `text`: the page as it is now.
/// `look`/`click`/`type`/`key`: the relay's hands, as `browser_challenge`
/// has them, on the guest page.
pub async fn guest(args: Value) -> String {
    // Someone other than the owner on a node that carries none of their
    // traffic (egress.rs): not opened here at all.
    if crate::egress::others().is_some() && crate::egress::mode() == crate::egress::Others::Refuse {
        return crate::egress::refusal(GUEST_OP);
    }
    match args["action"].as_str().unwrap_or_default() {
        "open" => {
            let url = args["url"].as_str().unwrap_or_default();
            // The proxy refuses the same addresses; this says so cleanly
            // before a browser starts.
            if let Err(why) = crate::urlguard::check(url).await {
                return format!("error: {why}");
            }
            // A box gets the memory a drawn Chrome needs before one starts.
            if let Some(key) = guest_box() {
                let raised = async {
                    crate::user_envs::ensure(&key).await?;
                    crate::user_envs::raise_memory(&key, BOX_BROWSER_MB).await
                };
                if let Err(e) = raised.await {
                    return format!("error: the guest browser's box: {e}");
                }
            }
            let opened = browser_action_in(Session::Guest, &["open", url], NAV_TIMEOUT).await;
            if opened.starts_with("error") {
                return opened;
            }
            // The check first, given its time to clear by itself: the page
            // read after it is the real one (or its PDF, downloaded).
            let standing = standing_challenge_in(Session::Guest).await;
            let text = guest_text().await;
            match standing {
                Some((c, at)) => format!("{}\n{text}", crate::challenge::marker_line(&c, &at)),
                None => text,
            }
        }
        "text" => guest_text().await,
        _ => challenge_act(Session::Guest, &args).await,
    }
}

async fn guest_text() -> String {
    if let Some(got) = pdf_download_in(Session::Guest).await {
        return got;
    }
    let out = browser_action_in(Session::Guest, &["get", "text", "body"], NAV_TIMEOUT).await;
    if out.starts_with("error") {
        return out;
    }
    let text = serde_json::from_str::<Value>(&out)
        .ok()
        .and_then(|v| v["text"].as_str().map(str::to_string))
        .unwrap_or(out);
    let url = page_state_in(Session::Guest)
        .await
        .map(|p| p.url)
        .unwrap_or_default();
    let text: String = text.chars().take(GUEST_TEXT_MAX).collect();
    format!("url: {url}\n\n{text}")
}

// ── the guest browser in a box (PLAN-challenge-relay.md D6) ──────────

/// The box a guest page opens in, when it does not open on this node. On
/// a node that sends other people's traffic through its proxy (`[web]
/// others = "proxy"`, egress.rs) the guest browser of a session that is
/// not the owner's runs in that session's own sandbox: its only way out
/// is the box's (Tor, on the VPS), and file:// shows nothing but the box.
/// There the box is the fence, not guestproxy: veles.nft refuses a box
/// every private and local address. `None`: this node's own guest
/// browser, behind guestproxy.
fn guest_box() -> Option<String> {
    let session = crate::egress::others()?;
    if !matches!(crate::egress::mode(), crate::egress::Others::Proxy { .. }) {
        return None;
    }
    crate::user_envs::zone_for(Some(&session), crate::user_envs::ZoneArg::Own)
        .key()
        .map(str::to_string)
}

/// The memory a box that runs the guest browser may use, in MB. A drawn
/// Chrome with a Cloudflare check open peaked at 470 MB in a box made like
/// a user's (VPS, 2026-10-13); a box's own default is 256 MB.
const BOX_BROWSER_MB: u64 = 1024;

/// What runs in the box for one agent-browser call: the box's own Xvfb,
/// started unless its lock file names a live process (the image has no
/// pgrep), then agent-browser with the call's arguments ("$@"). `setsid`
/// and no terminal, so the display outlives the exec that started it, as
/// agent-browser's daemon and its Chrome do. One exec per call.
const BOX_SCRIPT: &str = "up() { p=$(cat /tmp/.X99-lock 2>/dev/null | tr -d ' '); \
    [ -n \"$p\" ] && kill -0 \"$p\" 2>/dev/null && [ -S /tmp/.X11-unix/X99 ]; }; \
    up || { rm -f /tmp/.X99-lock /tmp/.X11-unix/X99; \
    setsid Xvfb :99 -screen 0 1280x900x24 -nolisten tcp </dev/null >/dev/null 2>&1 & \
    i=0; until up || [ $i -ge 50 ]; do sleep 0.1; i=$((i+1)); done; }; \
    up || { echo '{\"success\":false,\"error\":\"the box display (Xvfb) did not start\"}'; exit 1; }; \
    exec env DISPLAY=:99 agent-browser --json \"$@\"";

/// agent-browser's flags in a box: the node's guest flags without the
/// proxy (the box's way out is its own), and what a container needs.
/// Chrome refuses to run as root without `--no-sandbox` (the box is the
/// sandbox); a box's /dev/shm is 64 MB; it has no GPU, so WebGL renders
/// in software. WebRTC gets no UDP, as on the node (the box has none
/// anyway: veles.nft lets it TCP only).
fn box_flags(webdriver: bool) -> Vec<String> {
    let mut launch: Vec<&str> = (!webdriver)
        .then_some("--disable-blink-features=AutomationControlled")
        .into_iter()
        .collect();
    launch.extend([
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--enable-unsafe-swiftshader",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    ]);
    launch.extend(DRAWN_ARGS);
    [
        "--headed",
        "--session",
        "veles-guest",
        "--args",
        &launch.join(","),
        "--input-mode",
        "human",
    ]
    .map(str::to_string)
    .to_vec()
}

/// The exec's argv for one call in a box.
fn box_argv(webdriver: bool, argv: &[String]) -> Vec<String> {
    let mut all = vec![
        "sh".to_string(),
        "-c".to_string(),
        BOX_SCRIPT.to_string(),
        "veles-guest".to_string(),
    ];
    all.extend(box_flags(webdriver));
    all.extend_from_slice(argv);
    all
}

/// Run one agent-browser call in the box `key`, as [`run_cli`] runs it
/// on this node.
async fn box_cli(key: &str, argv: &[String], timeout_secs: u64) -> String {
    let all = box_argv(crate::config::live().web.browser_webdriver, argv);
    match crate::user_envs::exec_argv(key, &all, timeout_secs).await {
        Ok(o) if o.timed_out => format!("error: timeout ({timeout_secs}s)"),
        Ok(o) => format_ab_output(&o.stdout, &o.stderr),
        Err(e) => format!("error: the guest browser's box: {e}"),
    }
}

// ── a PDF in the browser, downloaded ─────────────────────────────────

/// Start fetching the PDF a page shows, from INSIDE the page: its
/// cookies, its IP and its TLS, which is what a solved challenge's
/// clearance is bound to. One eval may not outlast agent-browser's CDP
/// timeout (30 s, checked 2026-10-12), and a 60 MB scan can, so the fetch
/// runs on in the page and is polled ([`PDF_POLL_JS`]), then read out in
/// slices.
fn pdf_start_js() -> String {
    format!(
        "(() => {{ \
         if (document.contentType !== 'application/pdf') return JSON.stringify({{pdf: false}}); \
         const max = {max}; \
         const st = window.__velesPdf = {{done: false}}; \
         (async () => {{ try {{ \
           const r = await fetch(location.href, {{credentials: 'include'}}); \
           const len = Number(r.headers.get('content-length') || 0); \
           if (len > max) {{ st.error = 'too large'; st.size = len; return; }} \
           const b = new Uint8Array(await r.arrayBuffer()); \
           if (b.length > max) {{ st.error = 'too large'; st.size = b.length; return; }} \
           let s = ''; \
           for (let i = 0; i < b.length; i += 0x8000) \
             s += String.fromCharCode.apply(null, b.subarray(i, i + 0x8000)); \
           st.b64 = btoa(s); st.status = r.status; st.url = r.url || location.href; \
         }} catch (e) {{ st.error = String(e); }} finally {{ st.done = true; }} }})(); \
         return JSON.stringify({{pdf: true}}); }})()",
        max = crate::downloads::MAX_BYTES
    )
}

/// Where the page's fetch is: everything but the bytes.
const PDF_POLL_JS: &str = "JSON.stringify((s => ({done: s.done, error: s.error, size: s.size, \
    status: s.status, url: s.url, len: s.b64 ? s.b64.length : 0}))\
    (window.__velesPdf || {done: true, error: 'the page changed'}))";

/// Characters of the base64 one eval reads out.
const PDF_SLICE: usize = 16 * 1024 * 1024;

/// How long a PDF has to arrive in the page.
const PDF_SECS: u64 = 300;

/// An eval's value, as agent-browser returns it.
async fn eval_value(s: Session, js: &str) -> Option<Value> {
    let out = browser_action_in(s, &["eval", js], CLICK_TIMEOUT).await;
    let data: Value = serde_json::from_str(&out).ok()?;
    Some(data["result"].clone())
}

/// An eval whose value is a JSON string, parsed.
async fn eval_json(s: Session, js: &str) -> Option<Value> {
    serde_json::from_str(eval_value(s, js).await?.as_str()?).ok()
}

/// The PDF the page in `s` shows, downloaded: the result is its one
/// download line (downloads.rs), or an error. `None`: the page is not a
/// PDF (or could not say), and the caller reads it as a page.
async fn pdf_download_in(s: Session) -> Option<String> {
    let started = eval_json(s, &pdf_start_js()).await?;
    if started["pdf"] != true {
        return None;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(PDF_SECS);
    let state = loop {
        match eval_json(s, PDF_POLL_JS).await {
            Some(v) if v["done"] == true => break v,
            _ if std::time::Instant::now() > deadline => {
                return Some(format!(
                    "error: the PDF did not arrive in the browser within {PDF_SECS}s"
                ))
            }
            _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
        }
    };
    let cleanup = || eval_value(s, "delete window.__velesPdf; 'ok'");
    if let Some(e) = state["error"].as_str() {
        cleanup().await;
        return Some(match state["size"].as_u64() {
            Some(n) if e == "too large" => format!(
                "error: the page is a PDF of {}, over the {} a download may be",
                crate::downloads::human_size(n as usize),
                crate::downloads::human_size(crate::downloads::MAX_BYTES)
            ),
            _ => format!("error: the PDF could not be fetched in the page: {e}"),
        });
    }
    let len = state["len"].as_u64().unwrap_or(0) as usize;
    let mut b64 = String::with_capacity(len);
    for at in (0..len).step_by(PDF_SLICE) {
        let js = format!("window.__velesPdf.b64.slice({at}, {})", at + PDF_SLICE);
        match eval_value(s, &js).await {
            Some(Value::String(part)) => b64.push_str(&part),
            _ => {
                cleanup().await;
                return Some("error: the PDF could not be read out of the page".into());
            }
        }
    }
    cleanup().await;
    let bytes = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(&b64).ok()
    };
    let status = state["status"].as_u64().unwrap_or(0);
    let Some(bytes) = bytes.filter(|b| (200..300).contains(&status) && crate::downloads::is_pdf(b))
    else {
        return Some(format!(
            "error: the page's PDF did not download (HTTP {status})"
        ));
    };
    let url = state["url"].as_str().unwrap_or_default().to_string();
    let meta = crate::downloads::Meta {
        name: crate::downloads::name_from_url(&url),
        mime: "application/pdf".into(),
        url,
    };
    Some(match crate::downloads::stage(&bytes, &meta) {
        Ok(token) => crate::downloads::line(&token),
        Err(e) => format!("error: {e:#}"),
    })
}

/// Locate the agent-browser binary on PATH. No npx fallback: an
/// npx-resolved copy can drift from the tested version mid-session,
/// and a loud "not found" beats a silent protocol mismatch.
async fn find_browser() -> Option<String> {
    // PATH with the platform's separator and spellings: `agent-browser`
    // is an npm shim, `agent-browser.cmd` on Windows.
    crate::shell::which("agent-browser").map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the guest browser in a box (D6) ──────────────────────────────

    /// A scratch home whose config.toml says `body`, read live at once.
    fn config(home: &tempfile::TempDir, body: &str) {
        std::fs::write(home.path().join("config.toml"), body).expect("config");
        crate::config::invalidate_live();
    }

    /// On a node that proxies other people's traffic, a guest page of a
    /// session that is not the owner's opens in that session's own box;
    /// anywhere else, and for the owner, on this node behind guestproxy.
    #[tokio::test]
    async fn someone_elses_guest_page_opens_in_their_own_box() {
        use crate::acl::{Principal, Role};
        let (home, _guard) = crate::testenv::scratch_home();
        crate::acl::set_session_principal("d6-user", Principal::known("irc:d6", Role::User));
        crate::acl::set_session_principal("d6-owner", Principal::known("irc:o", Role::Owner));
        let own = crate::user_envs::zone_for(Some("d6-user"), crate::user_envs::ZoneArg::Own);
        config(&home, "[web]\nothers = \"proxy\"\n");
        assert_eq!(
            crate::egress::scope("d6-user", false, async { guest_box() })
                .await
                .as_deref(),
            own.key(),
            "their own box, the one their shell runs in"
        );
        assert_eq!(
            crate::egress::scope("d6-owner", false, async { guest_box() }).await,
            None
        );
        assert_eq!(guest_box(), None, "a call that is nobody's in particular");
        config(&home, "[web]\nothers = \"direct\"\n");
        assert_eq!(
            crate::egress::scope("d6-user", false, async { guest_box() }).await,
            None
        );
        crate::acl::clear_session_principal("d6-user");
        crate::acl::clear_session_principal("d6-owner");
    }

    /// On a node that carries none of their traffic, a guest page of
    /// someone other than the owner is not opened at all.
    #[tokio::test]
    async fn a_refusing_node_opens_no_guest_page() {
        use crate::acl::{Principal, Role};
        let (home, _guard) = crate::testenv::scratch_home();
        crate::acl::set_session_principal("d6-refused", Principal::known("irc:r", Role::User));
        config(&home, "[web]\nothers = \"refuse\"\n");
        for action in ["open", "look", "click", "text"] {
            let out = crate::egress::scope(
                "d6-refused",
                false,
                guest(serde_json::json!({"action": action, "url": "https://example.org/"})),
            )
            .await;
            assert!(out.contains("others = \"refuse\""), "{action}: {out}");
        }
        crate::acl::clear_session_principal("d6-refused");
    }

    // ── the browser tools for other people (D3) ──────────────────────

    /// Whose browser a `browser_*` call drives: the agent's own for the
    /// owner; for anyone else (and the owner's `/egress tor`) the guest
    /// browser in their box on a node that proxies their traffic, and on
    /// any other node none at all, so it never runs on the owner's.
    #[tokio::test]
    async fn the_browser_tools_drive_the_callers_browser_or_none() {
        use crate::acl::{Principal, Role};
        let (home, _guard) = crate::testenv::scratch_home();
        crate::acl::set_session_principal("d3-user", Principal::known("irc:d3", Role::User));
        crate::acl::set_session_principal("d3-admin", Principal::known("irc:a", Role::Admin));
        crate::acl::set_session_principal("d3-owner", Principal::known("irc:o", Role::Owner));
        let caller_in =
            |sid: &'static str, chosen: bool| crate::egress::scope(sid, chosen, async { caller() });
        config(&home, "[web]\nothers = \"proxy\"\n");
        assert_eq!(caller_in("d3-owner", false).await, Ok(Session::Agent));
        assert_eq!(caller(), Ok(Session::Agent), "an owner surface");
        assert_eq!(caller_in("d3-user", false).await, Ok(Session::Guest));
        assert_eq!(
            caller_in("d3-admin", false).await,
            Ok(Session::Guest),
            "an admin is not the owner: it once drove the owner's browser"
        );
        assert_eq!(
            caller_in("d3-owner", true).await,
            Ok(Session::Guest),
            "the owner's /egress tor"
        );
        config(&home, "[web]\nothers = \"refuse\"\n");
        let refused = caller_in("d3-user", false).await.unwrap_err();
        assert!(refused.contains("others = \"refuse\""), "{refused}");
        assert_eq!(caller_in("d3-owner", false).await, Ok(Session::Agent));
        config(&home, "[web]\nothers = \"direct\"\n");
        let refused = caller_in("d3-user", false).await.unwrap_err();
        assert!(
            refused.contains("only in their own sandbox box"),
            "{refused}"
        );
        for sid in ["d3-user", "d3-admin", "d3-owner"] {
            crate::acl::clear_session_principal(sid);
        }
    }

    /// Every browser tool answers someone else's call on a node with no
    /// box for them by refusing, before anything is opened; on a node
    /// with one, a page that is not public is refused before the box is
    /// asked, and no window is offered for it.
    #[tokio::test]
    async fn someone_elses_browser_call_is_refused_where_it_has_no_box() {
        use crate::acl::{Principal, Role};
        use crate::tools::Tool as _;
        let (home, _guard) = crate::testenv::scratch_home();
        crate::acl::set_session_principal("d3-refused", Principal::known("irc:r", Role::User));
        config(&home, "[web]\nothers = \"refuse\"\n");
        let calls: Vec<(Box<dyn crate::tools::Tool>, serde_json::Value)> = vec![
            (
                Box::new(BrowserTool),
                serde_json::json!({"url": "https://example.org/"}),
            ),
            (Box::new(BrowserSnapshotTool), serde_json::json!({})),
            (
                Box::new(BrowserClickTool),
                serde_json::json!({"ref": "@e1"}),
            ),
            (
                Box::new(BrowserTypeTool),
                serde_json::json!({"ref": "@e1", "text": "x"}),
            ),
            (
                Box::new(BrowserScrollTool),
                serde_json::json!({"direction": "down"}),
            ),
            (
                Box::new(BrowserPressTool),
                serde_json::json!({"key": "Enter"}),
            ),
            (Box::new(BrowserBackTool), serde_json::json!({})),
            (
                Box::new(BrowserConsoleTool),
                serde_json::json!({"expression": "1"}),
            ),
            (
                Box::new(BrowserChallengeTool),
                serde_json::json!({"action": "look"}),
            ),
            (
                Box::new(BrowserChallengeTool),
                serde_json::json!({"action": "window"}),
            ),
        ];
        for (tool, args) in &calls {
            let out = crate::egress::scope("d3-refused", false, tool.call(args.clone())).await;
            assert!(
                out.starts_with("error:") && out.contains("others = \"refuse\""),
                "{}: {out}",
                tool.name()
            );
        }
        config(&home, "[web]\nothers = \"proxy\"\n");
        for (url, why) in [
            (
                "file:///etc/hostname",
                "refused: file: is not a fetchable scheme",
            ),
            (
                "http://127.0.0.1:8642/api/sessions",
                "refused: 127.0.0.1 points at",
            ),
        ] {
            let out = crate::egress::scope(
                "d3-refused",
                false,
                BrowserTool.call(serde_json::json!({ "url": url })),
            )
            .await;
            assert!(
                out.starts_with("error: ") && out.contains(why),
                "{url}: {out}"
            );
        }
        for action in ["window", "unwindow"] {
            let out = crate::egress::scope(
                "d3-refused",
                false,
                BrowserChallengeTool.call(serde_json::json!({ "action": action })),
            )
            .await;
            assert!(out.contains("no window opens"), "{action}: {out}");
        }
        crate::acl::clear_session_principal("d3-refused");
    }

    /// What runs in the box: the display started first, then
    /// agent-browser with the guest's flags and no proxy (the box's own
    /// way out is the fence), as root in a container needs it.
    #[test]
    fn a_box_call_is_one_exec_with_the_box_flags() {
        let argv = box_argv(
            false,
            &["open".to_string(), "https://example.org/".to_string()],
        );
        assert_eq!(&argv[..2], ["sh", "-c"]);
        assert!(argv[2].contains("Xvfb :99") && argv[2].contains("agent-browser --json \"$@\""));
        assert_eq!(argv[3], "veles-guest", "$0 for the script");
        assert_eq!(&argv[argv.len() - 2..], ["open", "https://example.org/"]);
        let flags = argv[4..argv.len() - 2].join(" ");
        for want in [
            "--headed",
            "--session veles-guest",
            "--no-sandbox",
            "--disable-dev-shm-usage",
            "--disable-blink-features=AutomationControlled",
            "--input-mode human",
        ] {
            assert!(flags.contains(want), "{want} in {flags}");
        }
        assert!(!flags.contains("--proxy"), "no proxy in a box: {flags}");
        let declared = box_argv(true, &[]).join(" ");
        assert!(
            !declared.contains("AutomationControlled"),
            "[web] browser_webdriver puts the flag back: {declared}"
        );
    }

    /// The exact envelope agent-browser 0.27.0 emits, success arm.
    #[test]
    fn envelope_success_yields_data() {
        let out = format_ab_output(
            r#"{"success":true,"data":{"title":"Example Domain","url":"https://example.com/"},"error":null}"#,
            "",
        );
        assert!(out.contains("Example Domain"));
        // Null data still reports success in words.
        assert_eq!(
            format_ab_output(r#"{"success":true,"data":null}"#, ""),
            "ok"
        );
    }

    /// Failure must surface as `error: …`, not a raw JSON blob — the
    /// old bridge leaked the whole envelope and models could not tell
    /// success from failure.
    #[test]
    fn envelope_failure_is_error_prefixed() {
        let out = format_ab_output(
            r#"{"success":false,"data":null,"error":"Unknown ref: e9999"}"#,
            "",
        );
        assert_eq!(out, "error: Unknown ref: e9999");
    }

    /// The pre-fix call shape, verbatim: what the OLD dispatch produced
    /// against the real CLI. If a bridge regression ever reintroduces
    /// the invented protocol, this stays a useful record of the symptom.
    #[test]
    fn legacy_invented_shape_would_not_parse_as_success() {
        let out = format_ab_output(
            r#"{"error":"Unknown command: {\"action\":\"navigate\"}","success":false}"#,
            "",
        );
        assert!(out.starts_with("error:"), "got: {out}");
    }

    #[test]
    fn plain_and_empty_output_passthrough() {
        assert_eq!(format_ab_output("hello\n", ""), "hello");
        assert_eq!(format_ab_output("", "boom\n"), "boom");
        assert_eq!(format_ab_output("", ""), "(no output)");
    }

    /// LIVE: needs agent-browser and the network, so it is ignored by
    /// default. Run it on the browser's node:
    /// `cargo test -p veles-core --lib -- --ignored guest_browser_live`.
    /// The guest browser loads a public page through guestproxy, and the
    /// fence holds: file:// is refused before a browser starts, and
    /// loopback and the WireGuard peers come back as the proxy's refusal.
    #[tokio::test]
    #[ignore]
    async fn guest_browser_live() {
        let out = guest(serde_json::json!({"action": "open", "url": "https://example.com/"})).await;
        assert!(
            out.starts_with("url: https://example.com/") && out.contains("documentation examples"),
            "{out}"
        );
        let out = guest(serde_json::json!({"action": "open", "url": "file:///etc/hostname"})).await;
        assert!(out.starts_with("error"), "{out}");
        // Loopback and a WireGuard peer on a web port: refused for the
        // address. Loopback on another port: refused for the port.
        for (url, why) in [
            ("http://127.0.0.1/", "loopback"),
            ("http://10.0.30.1/", "a private network"),
            ("http://127.0.0.1:8888/", "not a web port"),
        ] {
            let _ = browser_action_in(Session::Guest, &["open", url], NAV_TIMEOUT).await;
            let text = guest_text().await;
            assert!(
                text.contains("veles guest browser") && text.contains(why),
                "{url}: {text}"
            );
        }
        let _ = browser_action_in(Session::Guest, &["close"], CLICK_TIMEOUT).await;
    }

    /// LIVE, like [`guest_browser_live`]: a PDF the guest browser opens
    /// is downloaded from inside the page, not read as its viewer, and
    /// delivered as the file itself.
    /// `cargo test -p veles-core --lib -- --ignored guest_pdf_live`.
    #[tokio::test]
    #[ignore]
    async fn guest_pdf_live() {
        let (_home, _env) = crate::testenv::scratch_home();
        let url = "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf";
        let out = guest(serde_json::json!({"action": "open", "url": url})).await;
        assert!(out.starts_with("[download "), "{out}");
        let got = crate::downloads::deliver("", out).await;
        let path = got
            .split(" to ")
            .nth(1)
            .and_then(|r| r.split(" where").next())
            .unwrap_or_default();
        let bytes = std::fs::read(path).unwrap_or_default();
        assert!(crate::downloads::is_pdf(&bytes), "{got}");
        let _ = browser_action_in(Session::Guest, &["close"], CLICK_TIMEOUT).await;
    }

    /// LIVE, against the page that started this (2026-10-12): Coventry's
    /// Pure portal puts Cloudflare's managed check in front of the PDF. The
    /// drawn guest browser is given the check's time to clear by itself,
    /// then downloads the file — nobody is asked. Headless, it was held.
    /// `cargo test -p veles-core --lib -- --ignored guest_coventry_live`.
    #[tokio::test]
    #[ignore]
    async fn guest_coventry_live() {
        let (_home, _env) = crate::testenv::scratch_home();
        let url = "https://pureportal.coventry.ac.uk/files/60371196/Published.pdf";
        let out = guest(serde_json::json!({"action": "open", "url": url})).await;
        let _ = browser_action_in(Session::Guest, &["close"], CLICK_TIMEOUT).await;
        assert!(!crate::challenge::marked(&out), "still held: {out}");
        let got = crate::downloads::deliver("", out).await;
        assert!(got.contains("/downloads/Published-"), "{got}");
    }

    /// LIVE, and heavy (65 MB): a large PDF outlasts one eval (30 s), so
    /// the page's fetch is polled and read out in slices. The USER: "some
    /// awful pdfs are like 60MB".
    /// `cargo test -p veles-core --lib -- --ignored guest_large_pdf_live`.
    #[tokio::test]
    #[ignore]
    async fn guest_large_pdf_live() {
        let (_home, _env) = crate::testenv::scratch_home();
        let url = "https://www.ipcc.ch/site/assets/uploads/sites/2/2022/06/SR15_Full_Report_HR.pdf";
        let out = guest(serde_json::json!({"action": "open", "url": url})).await;
        assert!(out.starts_with("[download "), "{out}");
        let got = crate::downloads::deliver("", out).await;
        assert!(got.contains("(65.0 MB, application/pdf)"), "{got}");
        let _ = browser_action_in(Session::Guest, &["close"], CLICK_TIMEOUT).await;
    }

    /// Both browsers launch without the automation flag unless
    /// `[web] browser_webdriver` asks for it, and always move the pointer
    /// like a hand. The guest keeps its fence either way.
    #[tokio::test]
    async fn launch_flags_follow_the_webdriver_toggle() {
        let has = |f: &[String], a: &str| f.iter().any(|x| x.contains(a));
        let agent = flags_for(Session::Agent, false, &Draw::Headless)
            .await
            .unwrap();
        assert!(has(&agent, "AutomationControlled"), "{agent:?}");
        assert!(agent
            .windows(2)
            .any(|w| w[0] == "--input-mode" && w[1] == "human"));
        let agent_on = flags_for(Session::Agent, true, &Draw::Headless)
            .await
            .unwrap();
        assert!(!has(&agent_on, "AutomationControlled"), "{agent_on:?}");
        assert!(
            !agent_on.iter().any(|x| x == "--args"),
            "no empty --args: {agent_on:?}"
        );
        for webdriver in [false, true] {
            let guest = flags_for(Session::Guest, webdriver, &Draw::Headless)
                .await
                .unwrap();
            assert!(has(&guest, "<-loopback>") && has(&guest, "disable_non_proxied_udp"));
            assert_eq!(has(&guest, "AutomationControlled"), !webdriver, "{guest:?}");
        }
    }

    /// A drawn browser launches headed, on X11, with its GPU; a window on
    /// this node's screen takes exactly the private display's flags, so
    /// the relay's calls while the window is open reuse it (agent-browser
    /// relaunches a browser whose launch flags change). Headless takes
    /// none of them.
    /// The drawn (Xvfb) path is unix-only (PLAN-windows.md §1).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_drawn_browser_is_headed_with_the_same_flags_anywhere() {
        let private = Draw::Private(":99".into());
        for s in [Session::Agent, Session::Guest] {
            let drawn = flags_for(s, false, &private).await.unwrap();
            assert_eq!(drawn[0], "--headed", "{drawn:?}");
            let launch = &drawn[drawn.iter().position(|f| f == "--args").unwrap() + 1];
            assert!(launch.contains("--ozone-platform=x11"), "{launch}");
            assert!(launch.contains("AutomationControlled"), "{launch}");
            assert_eq!(drawn, flags_for(s, false, &Draw::Real).await.unwrap());
            let headless = flags_for(s, false, &Draw::Headless).await.unwrap();
            assert!(!headless
                .iter()
                .any(|f| f == "--headed" || f.contains("ozone")));
        }
    }

    /// LIVE: needs Xvfb, agent-browser and Chrome. A browser on the
    /// private display shows a page what a person's browser shows: no
    /// `HeadlessChrome`, a screen at least as big as its window, and the
    /// GPU's WebGL where there is one. The headless one failed all three
    /// and Cloudflare held it (2026-10-12).
    /// `cargo test -p veles-core --lib -- --ignored private_display_live`.
    #[tokio::test]
    #[ignore]
    async fn private_display_live() {
        let draw = draw_for(Session::Guest).await;
        assert!(matches!(draw, Draw::Private(_)), "{draw:?}");
        let _ = browser_action_in(Session::Guest, &["open", "about:blank"], NAV_TIMEOUT).await;
        let js = "JSON.stringify({ua: navigator.userAgent, screen: [screen.width, \
            screen.height], outer: [outerWidth, outerHeight], gl: (() => { const g = \
            document.createElement('canvas').getContext('webgl'); return g ? \
            g.getParameter(g.getExtension('WEBGL_debug_renderer_info').UNMASKED_RENDERER_WEBGL) \
            : 'none'; })()})";
        let v = eval_json(Session::Guest, js)
            .await
            .expect("the page answers");
        let _ = browser_action_in(Session::Guest, &["close"], CLICK_TIMEOUT).await;
        let ua = v["ua"].as_str().unwrap_or_default();
        assert!(ua.contains("Chrome/") && !ua.contains("Headless"), "{v}");
        assert!(v["screen"][0].as_u64() >= v["outer"][0].as_u64(), "{v}");
        if gpu_args().contains(&"--use-angle=gl-egl") {
            let gl = v["gl"].as_str().unwrap_or_default();
            assert!(gl != "none" && !gl.contains("SwiftShader"), "{v}");
        }
    }

    #[test]
    fn snapshot_flags_follow_full() {
        assert_eq!(snapshot_argv(false), vec!["snapshot", "-i"]);
        assert_eq!(snapshot_argv(true), vec!["snapshot"]);
    }
}
