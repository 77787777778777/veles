//! Apps the agent builds for the person (PLAN-sessions-and-phone.md A4;
//! USER decisions D5, D6, 2026-10-11).
//!
//! "Program your home computer": the person asks, the agent writes a small
//! Tk app, and it runs on the phone beside the chat — a counter, a
//! shopping list, a timer for the bread. An app is a directory:
//!
//! ```text
//! <data dir>/apps/<name>/app.toml   title, description, entry, icon,
//!                                   capabilities
//!                         main.tcl  the entry script (Tk)
//!                         icon.png  optional
//! ```
//!
//! Apps run in the desktop window (D5), in a safe interpreter with Tk
//! (D6): they see their own widgets and a `veles` API, and every call into
//! that API is checked against the capabilities the app DECLARED and the
//! person GRANTED on its first run. A changed declaration asks again
//! ([`AppInfo::grant_key`]); changed code under the same declaration does
//! not — the declaration is the contract the person agreed to.
//!
//! This module is the gateway's half: listing, reading an app's files for
//! the window that runs it, a per-app store, and making a new app. The
//! window's half is veles-desktop/lib/apps.tcl.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Capabilities an app may declare. `http:<host>` names one host it may
/// fetch from.
pub const CAPABILITIES: &[(&str, &str)] = &[
    ("notify", "post notifications"),
    ("toast", "show short messages"),
    ("vibrate", "vibrate the phone"),
    ("speak", "read text aloud"),
    ("open", "open links"),
    ("share", "open the share sheet"),
    ("clipboard", "read and set the clipboard"),
    ("location", "know where the phone is"),
    ("store", "keep its own data"),
    ("ask", "ask the agent"),
];

/// The most an app's text files may weigh, together: an app is small.
const MAX_SOURCE_BYTES: u64 = 512 * 1024;
/// The most an app's store may weigh.
const MAX_STORE_BYTES: usize = 1024 * 1024;
const MAX_ICON_BYTES: u64 = 128 * 1024;

pub fn apps_dir() -> PathBuf {
    crate::config::data_dir().join("apps")
}

fn store_path(name: &str) -> PathBuf {
    crate::config::data_dir()
        .join("app-data")
        .join(format!("{name}.json"))
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
}

/// A declared capability that this veles knows.
pub fn known_capability(cap: &str) -> bool {
    if let Some(host) = cap.strip_prefix("http:") {
        return !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    }
    CAPABILITIES.iter().any(|(c, _)| *c == cap)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Manifest {
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

/// One app, as a window lists and runs it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct AppInfo {
    pub name: String,
    pub title: String,
    pub description: String,
    pub entry: String,
    pub icon: Option<String>,
    /// Declared, known, sorted.
    pub capabilities: Vec<String>,
    /// What the window keys a grant by: changes when the declaration does.
    pub grant_key: String,
    /// Declared but unknown here: refused, and said.
    pub unknown: Vec<String>,
}

fn sha_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A path inside `dir`, by a relative name that cannot climb out of it.
fn inside(dir: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(dir.join(p))
}

/// Read a regular file, never through a symlink.
fn read_plain(path: &Path, max: u64) -> Result<Vec<u8>> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("{} is not there", path.display()))?;
    if !meta.file_type().is_file() {
        bail!("{} is not a plain file", path.display());
    }
    if meta.len() > max {
        bail!("{} is larger than {max} bytes", path.display());
    }
    Ok(std::fs::read(path)?)
}

pub fn load(name: &str) -> Result<AppInfo> {
    if !valid_name(name) {
        bail!("an app name is 1-32 of a-z, 0-9 and -: {name:?}");
    }
    let dir = apps_dir().join(name);
    let text = String::from_utf8(read_plain(&dir.join("app.toml"), 64 * 1024)?)
        .context("app.toml is not text")?;
    let m: Manifest = toml::from_str(&text).with_context(|| format!("{name}/app.toml"))?;
    let entry = m.entry.clone().unwrap_or_else(|| "main.tcl".into());
    if inside(&dir, &entry).is_none() {
        bail!("{name}: entry {entry:?} is not a file in the app");
    }
    let (mut caps, mut unknown) = (Vec::new(), Vec::new());
    for c in m.capabilities {
        let c = c.trim().to_string();
        if known_capability(&c) {
            if !caps.contains(&c) {
                caps.push(c);
            }
        } else {
            unknown.push(c);
        }
    }
    caps.sort();
    Ok(AppInfo {
        name: name.to_string(),
        title: if m.title.trim().is_empty() {
            name.to_string()
        } else {
            m.title.trim().to_string()
        },
        description: m.description.trim().to_string(),
        grant_key: sha_hex(&format!("{name}\n{}", caps.join("\n"))),
        entry,
        icon: m.icon.filter(|i| inside(&dir, i).is_some()),
        capabilities: caps,
        unknown,
    })
}

/// Every app, in name order. One that does not load is listed with its
/// error, so a broken app is seen rather than vanishing.
pub fn list() -> Vec<std::result::Result<AppInfo, (String, String)>> {
    let mut names: Vec<String> = std::fs::read_dir(apps_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| valid_name(n))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .into_iter()
        .map(|n| load(&n).map_err(|e| (n, format!("{e:#}"))))
        .collect()
}

/// The app's text files (`.tcl`, `.toml`, `.txt`, `.json`, `.md`), by
/// relative path: what the window needs to run it. Subdirectories one
/// level deep; no symlinks; at most [`MAX_SOURCE_BYTES`] in all.
pub fn source(name: &str) -> Result<BTreeMap<String, String>> {
    let info = load(name)?;
    let dir = apps_dir().join(name);
    let mut out = BTreeMap::new();
    let mut total = 0u64;
    let mut walk = vec![(dir.clone(), String::new(), 0)];
    while let Some((d, prefix, depth)) = walk.pop() {
        for e in std::fs::read_dir(&d)?.filter_map(|e| e.ok()) {
            let ft = e.file_type()?;
            let Ok(fname) = e.file_name().into_string() else {
                continue;
            };
            let rel = format!("{prefix}{fname}");
            if ft.is_dir() && depth < 1 && !fname.starts_with('.') {
                walk.push((e.path(), format!("{rel}/"), depth + 1));
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let text_like = [".tcl", ".toml", ".txt", ".json", ".md"]
                .iter()
                .any(|x| fname.ends_with(x));
            if !text_like {
                continue;
            }
            let bytes = read_plain(&e.path(), MAX_SOURCE_BYTES)?;
            total += bytes.len() as u64;
            if total > MAX_SOURCE_BYTES {
                bail!("{name} is larger than {MAX_SOURCE_BYTES} bytes of text");
            }
            out.insert(rel, String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    if !out.contains_key(&info.entry) {
        bail!("{name}: its entry {} is missing", info.entry);
    }
    Ok(out)
}

/// The files an app's code may be: what [`source`] hands the window.
const TEXT_FILES: [&str; 5] = [".tcl", ".toml", ".txt", ".json", ".md"];

/// Write one of an app's text files (the `app` tool's `write`): inside
/// `apps/<name>/`, at most one folder down (what [`source`] reads),
/// never through a symlink, and an `app.toml` only when it declares
/// capabilities this veles knows. The person's grant is keyed by the
/// declaration, so a new one is asked again on the next run.
///
/// This is how the agent writes app code, rather than `write_file`: the
/// data dir is veles' own, so a file tool's write there asks the owner
/// every time (`approval::classify`, the whole home on a phone), and an
/// exemption for `apps/` would let any principal that can write files
/// change an app the person already granted. The `app` tool is the
/// owner's alone.
pub fn write_file(name: &str, rel: &str, content: &str) -> Result<PathBuf> {
    load(name)?;
    let dir = apps_dir().join(name);
    let rel = if rel.trim().is_empty() {
        "main.tcl"
    } else {
        rel.trim()
    };
    let Some(path) = inside(&dir, rel) else {
        bail!("{rel:?} is not a file in the app");
    };
    if Path::new(rel).components().count() > 2 {
        bail!("an app's files are in its folder or one folder below it: {rel:?}");
    }
    if !TEXT_FILES.iter().any(|x| rel.ends_with(x)) {
        bail!("an app's code is text: {}", TEXT_FILES.join(", "));
    }
    if content.len() as u64 > MAX_SOURCE_BYTES {
        bail!("{rel} would be larger than {MAX_SOURCE_BYTES} bytes");
    }
    if rel == "app.toml" {
        let m: Manifest = toml::from_str(content).context("app.toml")?;
        let unknown: Vec<&String> = m
            .capabilities
            .iter()
            .filter(|c| !known_capability(c.trim()))
            .collect();
        if !unknown.is_empty() {
            bail!(
                "unknown capabilities {unknown:?}; an app may declare: {}, http:<host>",
                CAPABILITIES
                    .iter()
                    .map(|(c, _)| *c)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    // No symlink anywhere from the app's folder to the file: a link in
    // the way would carry the write out of the app.
    let mut at = dir.clone();
    let real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_dir());
    if !real_dir(&at) {
        bail!("{} is not a plain folder", at.display());
    }
    let parts: Vec<_> = Path::new(rel).components().collect();
    for c in &parts[..parts.len() - 1] {
        at = at.join(c);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_dir() => {}
            Ok(_) => bail!("{} is not a plain folder", at.display()),
            Err(_) => std::fs::create_dir(&at)?,
        }
    }
    if let Ok(m) = std::fs::symlink_metadata(&path) {
        if !m.file_type().is_file() {
            bail!("{} is not a plain file", path.display());
        }
    }
    let tmp = path.with_file_name(format!(
        ".{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    ));
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Replace the one place `old` appears in an app's file with `new` (the
/// `app` tool's `edit`).
pub fn edit_file(name: &str, rel: &str, old: &str, new: &str) -> Result<PathBuf> {
    load(name)?;
    let rel = if rel.trim().is_empty() {
        "main.tcl"
    } else {
        rel.trim()
    };
    let Some(path) = inside(&apps_dir().join(name), rel) else {
        bail!("{rel:?} is not a file in the app");
    };
    if old.is_empty() {
        bail!("edit needs the text to replace (old)");
    }
    let text = String::from_utf8(read_plain(&path, MAX_SOURCE_BYTES)?)
        .with_context(|| format!("{rel} is not text"))?;
    match text.matches(old).count() {
        1 => write_file(name, rel, &text.replacen(old, new, 1)),
        0 => bail!("{rel} does not contain that text; read it and try again"),
        n => bail!("that text is in {rel} {n} times; give more of it so it is one place"),
    }
}

/// The app's icon as base64 PNG, when it has one.
pub fn icon_b64(info: &AppInfo) -> Option<String> {
    let rel = info.icon.as_deref()?;
    let path = inside(&apps_dir().join(&info.name), rel)?;
    let bytes = read_plain(&path, MAX_ICON_BYTES).ok()?;
    if !bytes.starts_with(b"\x89PNG") {
        return None;
    }
    use base64::Engine;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// A new app: its directory, an `app.toml` and a `main.tcl` that runs.
/// Refuses a name that exists.
pub fn create(name: &str, title: &str, capabilities: &[String]) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("an app name is 1-32 of a-z, 0-9 and -: {name:?}");
    }
    for c in capabilities {
        if !known_capability(c) {
            bail!(
                "{c:?} is not a capability; known: {}, http:<host>",
                CAPABILITIES
                    .iter()
                    .map(|(c, _)| *c)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let dir = apps_dir().join(name);
    if dir.exists() {
        bail!("an app called {name} exists already");
    }
    std::fs::create_dir_all(&dir)?;
    let title = if title.trim().is_empty() {
        name
    } else {
        title.trim()
    };
    let mut doc = toml_edit::DocumentMut::new();
    doc["title"] = toml_edit::value(title);
    doc["description"] = toml_edit::value("");
    doc["entry"] = toml_edit::value("main.tcl");
    let mut caps = toml_edit::Array::new();
    for c in capabilities {
        caps.push(c.as_str());
    }
    doc["capabilities"] = toml_edit::value(caps);
    std::fs::write(dir.join("app.toml"), doc.to_string())?;
    std::fs::write(
        dir.join("main.tcl"),
        format!(
            "# {title}: a veles app. It runs in a safe interpreter with Tk;\n\
             # `veles::` commands reach the phone and the agent (docs/apps.md).\n\
             ttk::label .hello -text {{{title}}}\n\
             pack .hello -padx 20 -pady 20\n"
        ),
    )?;
    Ok(dir)
}

// ── the per-app store ────────────────────────────────────────────────

fn read_store(name: &str) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(store_path(name))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// `get`, `set`, `delete` or `keys` on the app's own data: a JSON object
/// of string keys, kept beside the apps, not inside them, so rewriting an
/// app keeps what it stored.
pub fn store(name: &str, op: &str, key: &str, value: Option<Value>) -> Result<Value> {
    if !valid_name(name) {
        bail!("no such app");
    }
    let mut data = read_store(name);
    match op {
        "get" => Ok(data.get(key).cloned().unwrap_or(Value::Null)),
        "keys" => Ok(json!(data.keys().collect::<Vec<_>>())),
        "set" | "delete" => {
            if key.is_empty() || key.len() > 200 {
                bail!("a key is 1-200 characters");
            }
            if op == "set" {
                data.insert(key.to_string(), value.unwrap_or(Value::Null));
            } else {
                data.remove(key);
            }
            let text = serde_json::to_string(&data)?;
            if text.len() > MAX_STORE_BYTES {
                bail!("{name}'s store would be larger than {MAX_STORE_BYTES} bytes");
            }
            let path = store_path(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, text)?;
            std::fs::rename(&tmp, &path)?;
            Ok(Value::Bool(true))
        }
        _ => bail!("store ops are get, set, delete and keys"),
    }
}

/// An app as the window lists it: the info, its icon, or why it failed.
pub fn listing() -> Value {
    json!({
        "apps": list()
            .into_iter()
            .map(|r| match r {
                Ok(i) => {
                    let icon = icon_b64(&i);
                    let mut v = serde_json::to_value(&i).unwrap_or(Value::Null);
                    v["icon_png"] = json!(icon);
                    v
                }
                Err((name, e)) => json!({"name": name, "error": e}),
            })
            .collect::<Vec<_>>()
    })
}

// ── the agent's tool ─────────────────────────────────────────────────

/// `app` — the agent's dev loop for apps (A4): make one, write its files
/// (its own `write` and `edit`, under `apps/<name>/`; [`write_file`] says
/// why not the file tools), run it on a window, read
/// its errors, look at it, change it, run it again. The window that runs
/// it is a connected device (`crate::devices`): the desktop offers
/// `app.run`, `app.stop`, `app.reload`, `app.logs` and `app.screenshot`.
pub struct AppTool;

#[async_trait::async_trait]
impl crate::tools::Tool for AppTool {
    fn name(&self) -> &str {
        "app"
    }

    fn description(&self) -> &str {
        "Make and run small apps for the person: Tk scripts that run in the veles window on \
         their phone or laptop, each in a safe interpreter (no exec, files or wm: pack widgets \
         into `.`). list: the apps. new {name, title, \
         capabilities}: a new app (apps/<name>/app.toml + main.tcl in the data dir). write \
         {name, file?, content}: a whole file of the app (main.tcl unless file names another; \
         app.toml too). edit {name, file?, old, new}: replace the one place old appears. \
         Use these, not write_file, for an app's files. run / stop / reload {name, device?}: on a connected \
         window. logs {name}: its errors and veles::log lines. screenshot {name}: a picture of \
         its window, saved to a file for vision_analyze. Capabilities an app may declare: \
         notify, toast, vibrate, speak, open, share, clipboard, location, store, ask, \
         http:<host>; the person grants them on its first run. In the app, `veles::notify \
         title text`, `veles::store get|set|keys key ?value?`, `veles::ask text callback`, \
         `veles::log text`, and so on (docs/apps.md)."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "new", "write", "edit", "run", "stop", "reload", "logs", "screenshot"]},
                "name": {"type": "string"},
                "title": {"type": "string"},
                "file": {"type": "string", "description": "write/edit: the app's file, main.tcl unless given"},
                "content": {"type": "string"},
                "old": {"type": "string"},
                "new": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "device": {"type": "string", "description": "which window, when more than one runs apps"}
            },
            "required": ["action"]
        })
    }

    async fn call(&self, args: Value) -> String {
        let action = args["action"].as_str().unwrap_or("");
        let name = args["name"].as_str().unwrap_or("").trim();
        match action {
            "list" => {
                let all = list();
                if all.is_empty() {
                    return format!("No apps yet. `new` makes one in {}.", apps_dir().display());
                }
                let mut out = String::new();
                for r in all {
                    match r {
                        Ok(i) => out.push_str(&format!(
                            "- {} ({}): {} [{}]{}\n",
                            i.name,
                            i.title,
                            apps_dir().join(&i.name).display(),
                            i.capabilities.join(", "),
                            if i.unknown.is_empty() {
                                String::new()
                            } else {
                                format!(" — unknown capabilities: {}", i.unknown.join(", "))
                            }
                        )),
                        Err((n, e)) => out.push_str(&format!("- {n}: BROKEN: {e}\n")),
                    }
                }
                out
            }
            "new" => {
                let caps: Vec<String> = args["capabilities"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                match create(name, args["title"].as_str().unwrap_or(""), &caps) {
                    Ok(dir) => format!(
                        "made {}: app.toml and main.tcl. Write the app's code with \
                         action=write (Tk, in a safe interpreter), then run it.",
                        dir.display()
                    ),
                    Err(e) => format!("error: {e:#}"),
                }
            }
            "write" => {
                let Some(content) = args["content"].as_str() else {
                    return "error: write needs content: the whole file".into();
                };
                match write_file(name, args["file"].as_str().unwrap_or(""), content) {
                    Ok(p) => format!(
                        "wrote {} ({} bytes). Run or reload the app to see it.",
                        p.display(),
                        content.len()
                    ),
                    Err(e) => format!("error: {e:#}"),
                }
            }
            "edit" => {
                let (Some(old), Some(new)) = (args["old"].as_str(), args["new"].as_str()) else {
                    return "error: edit needs old (the text there now) and new".into();
                };
                match edit_file(name, args["file"].as_str().unwrap_or(""), old, new) {
                    Ok(p) => format!("edited {}. Run or reload the app to see it.", p.display()),
                    Err(e) => format!("error: {e:#}"),
                }
            }
            "run" | "stop" | "reload" | "logs" | "screenshot" => {
                if let Err(e) = load(name) {
                    return format!("error: {e:#}");
                }
                let body = json!({
                    "device": args["device"].as_str(),
                    "action": format!("app.{action}"),
                    "args": {"name": name},
                });
                let r = crate::desktop_tools::desktop_post(
                    "/api/device/call",
                    body,
                    std::time::Duration::from_secs(70),
                )
                .await;
                match r {
                    Ok(v) if v["ok"].as_bool() == Some(true) => {
                        let result = match &v["result"] {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        if action == "screenshot" {
                            save_shot(name, &result)
                        } else {
                            result
                        }
                    }
                    Ok(v) => format!(
                        "error: {}",
                        v["error"].as_str().unwrap_or("the window refused")
                    ),
                    Err(e) => format!("error: {e}"),
                }
            }
            _ => "error: action is one of list, new, write, edit, run, stop, reload, logs, screenshot"
                .into(),
        }
    }
}

/// The window's screenshot (base64 PNG) saved for `vision_analyze`.
fn save_shot(name: &str, b64: &str) -> String {
    use base64::Engine;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return format!(
            "error: the window's screenshot was not a picture: {}",
            b64.chars().take(200).collect::<String>()
        );
    };
    let dir = crate::config::data_dir().join("cache").join("app-shots");
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let path = dir.join(format!("{name}-{stamp}.png"));
    match std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, bytes)) {
        Ok(()) => format!(
            "screenshot of {name} saved: {} (look at it: vision_analyze with image_url={})",
            path.display(),
            path.display()
        ),
        Err(e) => format!("error: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_capabilities() {
        for n in ["counter", "bread-timer", "a1"] {
            assert!(valid_name(n), "{n}");
        }
        for n in ["", "Counter", "../x", "a/b", "-x", &"a".repeat(33)] {
            assert!(!valid_name(n), "{n}");
        }
        assert!(known_capability("notify") && known_capability("http:api.example.org"));
        assert!(
            !known_capability("exec")
                && !known_capability("http:")
                && !known_capability("http:a/b")
        );
        assert!(inside(Path::new("/a"), "main.tcl").is_some());
        assert!(inside(Path::new("/a"), "lib/x.tcl").is_some());
        for bad in ["../x", "/etc/passwd", "a/../../b", ""] {
            assert!(inside(Path::new("/a"), bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn an_app_is_made_listed_and_read() {
        let (_home, _g) = crate::testenv::scratch_home();
        let dir = create("counter", "Counter", &["store".into(), "notify".into()]).unwrap();
        assert!(create("counter", "x", &[]).is_err(), "names are unique");
        assert!(create("bad", "x", &["exec".into()]).is_err());
        let info = load("counter").unwrap();
        assert_eq!(info.title, "Counter");
        assert_eq!(info.capabilities, vec!["notify", "store"]);
        let src = source("counter").unwrap();
        assert!(src["main.tcl"].contains("ttk::label"), "{src:?}");
        assert!(src.contains_key("app.toml"));

        // The grant key follows the declaration, not the code.
        let before = info.grant_key.clone();
        std::fs::write(dir.join("main.tcl"), "pack [ttk::button .b -text hi]\n").unwrap();
        assert_eq!(load("counter").unwrap().grant_key, before);
        std::fs::write(
            dir.join("app.toml"),
            "title = \"Counter\"\ncapabilities = [\"store\", \"notify\", \"ask\", \"exec\"]\n",
        )
        .unwrap();
        let changed = load("counter").unwrap();
        assert_ne!(changed.grant_key, before, "a wider declaration asks again");
        assert_eq!(changed.unknown, vec!["exec"]);

        // A symlink out of the app is not read; a broken app is listed
        // with its error.
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", dir.join("leak.txt")).unwrap();
        assert!(!source("counter").unwrap().contains_key("leak.txt"));
        std::fs::create_dir_all(apps_dir().join("broken")).unwrap();
        let all = list();
        assert_eq!(all.len(), 2);
        assert!(all
            .iter()
            .any(|r| matches!(r, Err((n, _)) if n == "broken")));
        let l = listing();
        assert_eq!(l["apps"][0]["name"], "broken");
        assert!(l["apps"][0]["error"].is_string());
        assert_eq!(l["apps"][1]["title"], "Counter");
    }

    /// The agent writes an app's code through the `app` tool, inside the
    /// app's folder and nowhere else (the file tools would ask the owner
    /// at every write into the data dir).
    #[tokio::test]
    async fn the_tool_writes_and_edits_only_inside_the_app() {
        use crate::tools::Tool;
        let (home, _g) = crate::testenv::scratch_home();
        // `home` feeds the unix-only symlink leg below.
        let _ = &home;
        let dir = create("tally", "Tally", &[]).unwrap();
        let t = AppTool;
        let r = t
            .call(json!({"action": "write", "name": "tally", "content": "set n 0\npack [ttk::label .l -textvariable n]\n"}))
            .await;
        assert!(r.starts_with("wrote "), "{r}");
        assert!(std::fs::read_to_string(dir.join("main.tcl"))
            .unwrap()
            .starts_with("set n 0"));
        let r = t
            .call(json!({"action": "edit", "name": "tally", "old": "set n 0", "new": "set n 5"}))
            .await;
        assert!(r.starts_with("edited "), "{r}");
        assert!(std::fs::read_to_string(dir.join("main.tcl"))
            .unwrap()
            .starts_with("set n 5"));
        // One folder down is the app's too; source reads it.
        write_file("tally", "lib/util.tcl", "proc twice {x} {expr {2*$x}}\n").unwrap();
        assert!(source("tally").unwrap().contains_key("lib/util.tcl"));

        // Out of the app, or not an app's kind of file: refused.
        for (file, why) in [
            ("../escape.tcl", "is not a file in the app"),
            ("/etc/x.tcl", "is not a file in the app"),
            ("a/b/c.tcl", "one folder below"),
            ("run.sh", "is text"),
        ] {
            let e = write_file("tally", file, "x").unwrap_err().to_string();
            assert!(e.contains(why), "{file}: {e}");
        }
        assert!(!apps_dir().join("escape.tcl").exists());
        assert!(
            write_file("nosuch", "main.tcl", "x").is_err(),
            "the app must exist"
        );
        let e = t
            .call(
                json!({"action": "edit", "name": "tally", "old": "nothing like this", "new": "x"}),
            )
            .await;
        assert!(e.contains("does not contain"), "{e}");
        let e = t
            .call(json!({"action": "edit", "name": "tally", "file": "lib/util.tcl", "old": "x", "new": "y"}))
            .await;
        assert!(e.contains("times"), "{e}");

        // An app.toml is checked before it lands: the old one stays.
        let e = write_file("tally", "app.toml", "capabilities = [\"exec\"]\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown capabilities"), "{e}");
        assert!(write_file("tally", "app.toml", "title = [").is_err());
        assert_eq!(load("tally").unwrap().title, "Tally");
        write_file(
            "tally",
            "app.toml",
            "title = \"Tally\"\ncapabilities = [\"store\"]\n",
        )
        .unwrap();
        assert_eq!(load("tally").unwrap().capabilities, vec!["store"]);

        // A symlink in the way never carries a write out of the app.
        #[cfg(unix)]
        {
            let outside = home.path().join("outside.tcl");
            std::fs::write(&outside, "kept\n").unwrap();
            std::os::unix::fs::symlink(&outside, dir.join("linked.tcl")).unwrap();
            let e = write_file("tally", "linked.tcl", "x")
                .unwrap_err()
                .to_string();
            assert!(e.contains("not a plain file"), "{e}");
            let away = home.path().join("away");
            std::fs::create_dir(&away).unwrap();
            std::os::unix::fs::symlink(&away, dir.join("sub")).unwrap();
            let e = write_file("tally", "sub/x.tcl", "x")
                .unwrap_err()
                .to_string();
            assert!(e.contains("not a plain folder"), "{e}");
            assert_eq!(std::fs::read_to_string(&outside).unwrap(), "kept\n");
            assert!(!away.join("x.tcl").exists());
        }
    }

    #[test]
    fn an_apps_store_is_its_own() {
        let (_home, _g) = crate::testenv::scratch_home();
        assert_eq!(store("counter", "get", "n", None).unwrap(), Value::Null);
        store("counter", "set", "n", Some(json!(3))).unwrap();
        store("other", "set", "n", Some(json!("x"))).unwrap();
        assert_eq!(store("counter", "get", "n", None).unwrap(), json!(3));
        assert_eq!(store("counter", "keys", "", None).unwrap(), json!(["n"]));
        store("counter", "delete", "n", None).unwrap();
        assert_eq!(store("counter", "get", "n", None).unwrap(), Value::Null);
        assert_eq!(store("other", "get", "n", None).unwrap(), json!("x"));
        assert!(store("../x", "get", "n", None).is_err());
        assert!(store("counter", "set", "", Some(json!(1))).is_err());
        let big = "x".repeat(MAX_STORE_BYTES);
        assert!(store("counter", "set", "big", Some(json!(big))).is_err());
    }
}
