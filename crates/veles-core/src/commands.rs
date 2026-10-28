//! Custom slash commands from Markdown files (S6, Letta .commands parity).
//!
//! Locations (project wins over personal, matching Letta's precedence):
//!   1. `./.veles/commands/*.md`        — project, shared via the repo
//!   2. `~/.config/veles/commands/*.md` — personal, all projects
//!      (`$VELES_HOME/commands/` under a forced home)
//!
//! File name (minus `.md`) = the command: `/fix-issue 123` loads
//! `fix-issue.md`. Body supports:
//!   - `$ARGUMENTS` — everything after the command name
//!   - `$1`..`$9`   — positional words
//!   - `!`cmd``     — shell execution, output spliced in (backtick form)
//!   - `@path`      — file contents inlined
//!   - frontmatter  — `description` (+ `argument-hint` future)
//!
//! The RENDERED text becomes the user prompt for a normal agent turn —
//! commands are prompt templates, not code. Surfaces decide what to do
//! with it (CLI/TUI/desktop start a turn; the gateway treats it as the
//! inbound text).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    /// Raw body (frontmatter stripped).
    pub body: String,
    pub source: PathBuf,
}

/// Parse a command file into name + description + body.
fn parse_file(path: &Path) -> Option<CustomCommand> {
    let raw = std::fs::read_to_string(path).ok()?;
    let name = path.file_stem()?.to_string_lossy().to_string();
    // name sanity: must look like a slash command word
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let (description, body) = split_frontmatter(&raw);
    Some(CustomCommand {
        name,
        description,
        body: body.trim().to_string(),
        source: path.to_path_buf(),
    })
}

/// Minimal YAML frontmatter split: leading `---` block, `key: value`
/// lines. Only `description` is consumed today.
fn split_frontmatter(raw: &str) -> (String, String) {
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (String::new(), raw.to_string());
    }
    let mut description = String::new();
    let mut body_lines: Vec<&str> = Vec::new();
    let mut in_fm = true;
    for line in lines {
        if in_fm {
            if line.trim() == "---" {
                in_fm = false; // closing fence — body starts next line
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().trim_matches('"').to_string();
            }
        } else {
            body_lines.push(line);
        }
    }
    let body = if in_fm {
        // No closing fence: treat everything after the opening as body
        // (degenerate file — don't lose content).
        String::new()
    } else {
        body_lines.join("\n")
    };
    (description, body)
}

/// Discover all custom commands (project dir overrides personal).
pub fn discover() -> Vec<CustomCommand> {
    let mut by_name: HashMap<String, CustomCommand> = HashMap::new();
    for root in roots() {
        let dir = root.join("commands");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(c) = parse_file(&p) {
                by_name.insert(c.name.clone(), c); // later root wins
            }
        }
    }
    let mut v: Vec<CustomCommand> = by_name.into_values().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Command roots: personal first, project LAST (insert order makes the
/// last root win — the documented "project overrides personal").
///
/// The personal root is the CONFIG root: `commands/` is something the
/// user writes, so it lives under `~/.config/veles` beside
/// `config.toml`. This used to read
/// `veles_home()` — the DATA root — which on a split box is a
/// directory nothing else populates, and a command dropped where the
/// migration says it goes was silently never offered (the same
/// straggler shape `9946f92` swept out of the plugins read side).
fn roots() -> Vec<PathBuf> {
    let mut v = vec![crate::config::config_dir()];
    v.push(PathBuf::from(".veles")); // project last → wins
    v
}

/// Find one command by name.
pub fn find(name: &str) -> Option<CustomCommand> {
    let name = name.trim_start_matches('/');
    discover().into_iter().find(|c| c.name == name)
}

/// Render a command body with arguments filled in.
///
/// `args` = the raw text after the command name ("" when bare).
/// Expansion failures (missing file, shell error) splice a visible
/// error note instead of aborting — the model can react to it.
pub fn render(cmd: &CustomCommand, args: &str) -> String {
    let mut out = cmd.body.clone();
    let words: Vec<&str> = args.split_whitespace().collect();

    // Positional $1..$9 (do before $ARGUMENTS; both are plain
    // substitutions — no shell semantics, safe by construction).
    for i in 1..=9usize {
        let ph = format!("${i}");
        let val = words.get(i - 1).copied().unwrap_or("");
        out = out.replace(&ph, val);
    }
    out = out.replace("$ARGUMENTS", args.trim());

    // @file references — word-boundary tokens.
    out = inline_files(&out);

    // !`shell command` — backtick-bang form. Bounded output.
    out = expand_shell(&out);

    out.trim().to_string()
}

fn inline_files(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("@") {
        let (head, tail) = rest.split_at(pos);
        out.push_str(head);
        // Take the path token after @ (path chars, no spaces).
        let path_tok: String = tail[1..]
            .chars()
            .take_while(|c| !c.is_whitespace() && *c != '`')
            .collect();
        if path_tok.is_empty() {
            out.push('@');
            rest = &tail[1..];
            continue;
        }
        match std::fs::read_to_string(&path_tok) {
            Ok(content) => {
                out.push_str(&format!(
                    "───── {path_tok} ─────\n{}\n───── end {path_tok} ─────",
                    content.trim()
                ));
            }
            Err(e) => out.push_str(&format!("[@{path_tok}: unreadable — {e}]")),
        }
        rest = &tail[1 + path_tok.len()..];
    }
    out.push_str(rest);
    out
}

fn expand_shell(text: &str) -> String {
    // !`cmd` — bang immediately before a backticked command.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find("!`") {
            Some(pos) => {
                let (head, tail) = rest.split_at(pos);
                out.push_str(head);
                let Some(endrel) = tail[2..].find('`') else {
                    out.push_str(tail);
                    return out;
                };
                let cmd = &tail[2..2 + endrel];
                match std::process::Command::new(crate::shell::shell_binary())
                    .arg("-c")
                    .arg(cmd)
                    .output()
                {
                    Ok(o) => {
                        let mut s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                        if s.is_empty() && !err.is_empty() {
                            s = format!("[stderr] {err}");
                        }
                        // Bound the splice so a chatty command can't
                        // blow the prompt.
                        if s.len() > 4000 {
                            s.truncate(4000);
                            s.push_str("\n…[truncated]");
                        }
                        out.push_str(&s);
                    }
                    Err(e) => out.push_str(&format!("![{cmd}: {e}]")),
                }
                rest = &tail[2 + endrel + 1..];
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

// ── Tcl-registered commands (plugins) ───────────────────────────────
//
// A plugin can add a slash command whose body is code rather than a .md
// template. These live in memory for the process lifetime — a reload
// re-registers them the same way hook handlers re-register.
//
// Two bodies, because there are two plugin languages. The Tcl one
// (`veles::register_command`) names a PROC and renders through the
// interpreter thread; the native one (`register_command` in the plugin
// ABI, and in-tree Rust plugins) is a closure called directly. This
// used to be `TclCommand` with a bare `procname` field, which is why
// ABI v1 had no way to register a command at all: the registry could
// not hold anything that was not a proc name.

/// How a plugin-registered slash command produces its prompt text.
#[derive(Clone)]
pub enum CommandBody {
    /// A proc in the plugins interpreter. It reads
    /// `::veles::command_args` (the raw text after the command name)
    /// and returns the prompt text. Rendering hops to the Tcl thread.
    TclProc(String),
    /// A native render: args in, prompt text out. SYNC by the plugin
    /// contract, so callers run it where blocking is allowed — which
    /// for a `.so` command means it may itself call back into the
    /// engine through `host_call`.
    Native(Arc<dyn Fn(&str) -> String + Send + Sync>),
    /// A command that ANSWERS, like a builtin: the text comes back to
    /// the caller as the reply and is never a prompt (R5, a module's
    /// `/slopdrop`). Async, with the caller's principal and surface
    /// context, gated by its own capability floor ([`run_direct`]).
    Direct(DirectCommand),
}

impl std::fmt::Debug for CommandBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TclProc(p) => write!(f, "TclProc({p})"),
            Self::Native(_) => write!(f, "Native(<fn>)"),
            Self::Direct(d) => write!(f, "Direct(cap {})", d.cap.as_str()),
        }
    }
}

/// What a direct command is called with.
#[derive(Debug, Clone, Default)]
pub struct DirectCall {
    /// The raw text after the command name ("" when bare).
    pub args: String,
    /// The resolved caller; `None` on a local owner surface.
    pub principal: Option<crate::acl::Principal>,
    /// Where the caller is, when the surface knows.
    pub sender: Option<crate::acl::SenderContext>,
    /// The session the command was typed in; empty without one.
    pub session_id: String,
}

pub type DirectFuture = std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>>;

/// A direct command's body and its capability floor.
#[derive(Clone)]
pub struct DirectCommand {
    /// The capability a caller needs to reach the command at all; the
    /// body re-checks finer ones per verb.
    pub cap: crate::acl::Cap,
    pub run: Arc<dyn Fn(DirectCall) -> DirectFuture + Send + Sync>,
}

/// Run a direct command for `call`, after its capability floor. Every
/// surface answers it through here, so the gate cannot drift between
/// them.
pub async fn run_direct(name: &str, cmd: &DirectCommand, call: DirectCall) -> String {
    if let Some(p) = &call.principal {
        if !p.has(cmd.cap) {
            return format!(
                "[acl] /{name} requires the `{}` capability — denied for {}.",
                cmd.cap.as_str(),
                p.label()
            );
        }
    }
    (cmd.run)(call).await
}

/// The direct command named, if a plugin registered one.
pub fn direct_command(name: &str) -> Option<DirectCommand> {
    match plugin_command(name)?.body {
        CommandBody::Direct(d) => Some(d),
        _ => None,
    }
}

/// A slash command registered by a plugin, in either language.
#[derive(Debug, Clone)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
    pub body: CommandBody,
}

impl PluginCommand {
    /// The Tcl spelling, kept as a constructor because that is how
    /// `veles::register_command` builds one.
    pub fn tcl(name: &str, description: &str, procname: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            body: CommandBody::TclProc(procname.to_string()),
        }
    }

    /// What the plugin manager shows in the "how" column. For a Tcl
    /// command that is the proc; a native one has no name to show.
    pub fn origin(&self) -> String {
        match &self.body {
            CommandBody::TclProc(p) => format!("proc {p}"),
            CommandBody::Native(_) => "native".to_string(),
            CommandBody::Direct(d) => format!("direct ({})", d.cap.as_str()),
        }
    }
}

/// The plugin-command registry. OWNER-TRACKED (M4/R2): each command
/// records the registering thread's owner, so a module's commands can be
/// purged on reload without touching core's or another module's. Core/Tcl
/// registrations happen on untagged threads → owner `None`, never purged.
static PLUGIN_COMMANDS: std::sync::OnceLock<crate::owned::OwnedRegistry<String, PluginCommand>> =
    std::sync::OnceLock::new();

fn plugin_commands_cell() -> &'static crate::owned::OwnedRegistry<String, PluginCommand> {
    PLUGIN_COMMANDS.get_or_init(crate::owned::OwnedRegistry::default)
}

/// The same sanity `parse_file` applies to `.md` file names — one
/// spelling so a registered command and a discovered one cannot
/// disagree about what a command name is.
pub fn valid_command_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Register (or replace) a Tcl-backed command. Names are validated so
/// the registry can never hold something `find_any` would not accept.
/// The owner is read from the calling thread (M4).
pub fn register_command(cmd: PluginCommand) -> Result<(), String> {
    if !valid_command_name(&cmd.name) {
        return Err(format!(
            "invalid command name {:?} (letters, digits, -, _ only)",
            cmd.name
        ));
    }
    plugin_commands_cell().insert(cmd.name.clone(), cmd);
    Ok(())
}

/// Remove every command `owner` registered (R3 teardown on a module
/// reload). `None` (core) removes nothing. Returns the count removed.
pub fn purge_owner_commands(owner: Option<&'static str>) -> usize {
    plugin_commands_cell().purge(owner)
}

/// Drop one plugin command `owner` registered (an ABI 4 plugin's
/// `unregister`); other owners' same-named commands stay.
pub fn remove_owned_command(name: &str, owner: Option<&'static str>) -> bool {
    plugin_commands_cell().remove_owned(&name.to_string(), owner)
}

pub fn plugin_command(name: &str) -> Option<PluginCommand> {
    let name = name.trim_start_matches('/').to_string();
    plugin_commands_cell().with(&name, |c| c.clone())
}

pub fn plugin_commands() -> Vec<PluginCommand> {
    plugin_commands_cell().values_cloned()
}

/// What `find` resolves to once plugins can register commands too.
/// Plugin-registered commands WIN over a same-named `.md` file: the
/// live, explicit registration is the more intentional definition.
pub enum ResolvedCommand {
    Disk(CustomCommand),
    Plugin(PluginCommand),
}

pub fn find_any(name: &str) -> Option<ResolvedCommand> {
    let name = name.trim_start_matches('/');
    if let Some(t) = plugin_command(name) {
        return Some(ResolvedCommand::Plugin(t));
    }
    find(name).map(ResolvedCommand::Disk)
}

/// What a renderer says of a direct command: it is answered by the
/// surface's dispatcher (`run_direct`), never expanded into a prompt.
pub const DIRECT_NOT_A_PROMPT: &str =
    "a direct command answers the caller; it is not a prompt to render";

/// Render a plugin command from a SYNC surface (REPL, TUI, chat).
///
/// A Tcl body blocks the calling thread on the Tcl hop —
/// `TclHandle::set_eval` refuses if that thread happens to sit inside a
/// tokio runtime, which is the correct failure: async callers must use
/// `render_plugin_async`. Args land VERBATIM in
/// `::veles::command_args` (SetEval — arbitrary user text, no quoting
/// layer to corrupt it).
///
/// A native body is just called. It is sync by contract, and this
/// surface is already a blocking one, so it may reach back into the
/// engine.
pub fn render_plugin_blocking(cmd: &PluginCommand, args: &str) -> Result<String, String> {
    match &cmd.body {
        CommandBody::Native(f) => Ok(f(args)),
        CommandBody::Direct(_) => Err(DIRECT_NOT_A_PROMPT.to_string()),
        CommandBody::TclProc(procname) => {
            let handle = crate::tclrt::global_handle().ok_or_else(|| {
                "tcl runtime not running — plugin command unavailable".to_string()
            })?;
            handle.set_eval("::veles::command_args", args, procname)
        }
    }
}

/// Render a plugin command from an ASYNC surface (gateway, desktop API).
///
/// A Tcl body sends a SetEval request and AWAITS the reply — never
/// `blocking_recv` inside the runtime (the 2026-08-22 incident). A
/// native body goes to `spawn_blocking`, for the same reason: it is
/// sync, it may block on an engine call, and this is an executor
/// thread.
pub async fn render_plugin_async(cmd: &PluginCommand, args: &str) -> Result<String, String> {
    match &cmd.body {
        CommandBody::Direct(_) => Err(DIRECT_NOT_A_PROMPT.to_string()),
        CommandBody::Native(f) => {
            let f = Arc::clone(f);
            let args = args.to_string();
            crate::rt::spawn_blocking(move || f(&args))
                .await
                .map_err(|e| format!("plugin command panicked or was cancelled: {e}"))
        }
        CommandBody::TclProc(procname) => {
            let handle = crate::tclrt::global_handle().ok_or_else(|| {
                "tcl runtime not running — plugin command unavailable".to_string()
            })?;
            let (tx, rx) = tokio::sync::oneshot::channel();
            handle
                .send(crate::tclrt::TclRequest::SetEval {
                    var: "::veles::command_args".into(),
                    value: args.to_string(),
                    code: procname.clone(),
                    reply: tx,
                })
                .map_err(|_| "tcl thread dead".to_string())?;
            rx.await.map_err(|_| "tcl thread dropped".to_string())?
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// `run_direct` is the ONE gate every surface answers a direct
    /// command through: a caller below the command's floor is refused
    /// before the body runs, and a local owner surface (no principal)
    /// reaches it. The body here checks nothing itself, so this pins the
    /// floor alone.
    #[tokio::test]
    async fn run_direct_enforces_the_commands_floor_before_the_body() {
        let ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let r2 = ran.clone();
        let cmd = DirectCommand {
            cap: crate::acl::Cap::All,
            run: Arc::new(move |call: DirectCall| {
                let r = r2.clone();
                Box::pin(async move {
                    r.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    format!("ran with {:?}", call.args)
                })
            }),
        };
        let guest = crate::acl::Principal::known("irc:nobody", crate::acl::Role::Guest);
        let out = run_direct(
            "floor-test",
            &cmd,
            DirectCall {
                args: "x".into(),
                principal: Some(guest),
                ..Default::default()
            },
        )
        .await;
        assert!(out.starts_with("[acl] /floor-test requires"), "{out}");
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the body never ran"
        );
        let out = run_direct(
            "floor-test",
            &cmd,
            DirectCall {
                args: "x".into(),
                ..Default::default()
            },
        )
        .await;
        assert_eq!(out, "ran with \"x\"");
        assert_eq!(ran.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn frontmatter_and_body_split() {
        let raw = "---\ndescription: Review stuff\n---\n\nReview this:\n$ARGUMENTS\n";
        let (desc, body) = split_frontmatter(raw);
        assert_eq!(desc, "Review stuff");
        assert!(body.contains("$ARGUMENTS"));
        assert!(!body.contains("description"));
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let raw = "Just a body\n";
        let (desc, body) = split_frontmatter(raw);
        assert!(desc.is_empty());
        assert_eq!(body, raw);
    }

    #[test]
    fn positional_and_arguments() {
        let c = CustomCommand {
            name: "t".into(),
            description: String::new(),
            body: "fix $1 with $2 — all: $ARGUMENTS".into(),
            source: PathBuf::new(),
        };
        assert_eq!(
            render(&c, "456 high priority"),
            "fix 456 with high — all: 456 high priority"
        );
    }

    #[test]
    fn shell_expansion_bounded() {
        let c = CustomCommand {
            name: "t".into(),
            description: String::new(),
            body: "branch: !`echo trunk`".into(),
            source: PathBuf::new(),
        };
        assert_eq!(render(&c, ""), "branch: trunk");
    }

    #[test]
    fn shell_failure_is_visible_not_fatal() {
        let c = CustomCommand {
            name: "t".into(),
            description: String::new(),
            body: "x !`exit 3` y".into(),
            source: PathBuf::new(),
        };
        let r = render(&c, "");
        assert!(r.contains("x") && r.contains("y"), "body survives: {r}");
    }

    #[test]
    fn file_inline_missing_is_visible() {
        let c = CustomCommand {
            name: "t".into(),
            description: String::new(),
            body: "see @/nonexistent/definitely/not/here.md".into(),
            source: PathBuf::new(),
        };
        let r = render(&c, "");
        assert!(r.contains("unreadable"), "must say unreadable: {r}");
    }

    mod xdg_roots {
        use super::super::*;

        /// A personal command dropped in `~/.config/veles/commands/` is
        /// FOUND: `commands/` is config, so that is where a personal
        /// command lives. `roots()` used to read the DATA root —
        /// the same XDG-split straggler shape `9946f92` swept out of
        /// plugins — and the command was silently never offered.
        #[test]
        fn a_personal_command_lives_in_the_config_root() {
            let (cfg_root, _data_root, _g) = crate::testenv::scratch_xdg();
            std::fs::create_dir_all(cfg_root.path().join("veles/commands")).unwrap();
            std::fs::write(
                cfg_root.path().join("veles/commands/hello.md"),
                "personal hello",
            )
            .unwrap();
            let found = discover();
            assert!(
                found.iter().any(|c| c.name == "hello"),
                "the config-root command must be discovered: {:?}",
                found.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
            );
        }

        /// The DATA root is not a command root at all — not a fallback,
        /// not a secondary. If it were, a migration that copied rather
        /// than moved would let the stale data-root copy SHADOW the
        /// edited config one (or vice versa, per root order), and the
        /// user's edit would appear to do nothing. One root per half.
        #[test]
        fn the_data_root_is_not_a_command_root() {
            let (_cfg_root, data_root, _g) = crate::testenv::scratch_xdg();
            std::fs::create_dir_all(data_root.path().join("veles/commands")).unwrap();
            std::fs::write(
                data_root.path().join("veles/commands/stale.md"),
                "stale copy from a copy-migration",
            )
            .unwrap();
            let found = discover();
            assert!(
                !found.iter().any(|c| c.name == "stale"),
                "a file in the data root must never become a command: {:?}",
                found.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
            );
        }
    }
}
