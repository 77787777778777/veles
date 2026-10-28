//! Surgical `config.toml` edits — the write half the UI never had.
//!
//! Every "manager" surface in the desktop used to end at a sentence
//! telling you to go and edit `config.toml` by hand, because there was
//! no way to write it back. There is one now, and it obeys the rule
//! `save_failover` established when it wrote the first config section:
//!
//! **Never re-serialize.** Re-emitting a hand-written file from
//! `VelesConfig` would destroy the user's comments, their key order and
//! their formatting — so this edits the parsed *document* in place
//! (`toml_edit` keeps every trivia byte) and touches only the keys
//! asked for. `splice_failover`, the line-walking ancestor of this
//! module, did the same thing for exactly one section; this does it for
//! any path.
//!
//! Three safety properties, all load-bearing:
//!
//! 1. The result must still deserialize as `VelesConfig`. A config the
//!    agent cannot read is worse than an unsaved edit, so a mutation
//!    that would break the file is refused and nothing is written.
//! 2. The previous file is copied to `config.toml.bak` first.
//! 3. The caller is told what the write actually DID — see `Effect`.
//!    Some sections are re-read on every use and take effect
//!    immediately; others are held from boot. Saying "saved" without
//!    saying which is how a settings pane earns distrust.
//!
//! Unknown keys survive: `VelesConfig` does not `deny_unknown_fields`,
//! and the document is never rebuilt from it — so the desktop-only
//! `[ui]` block (`coach`, `font_scale`, read directly by the Tcl side)
//! passes through untouched.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use toml_edit::{DocumentMut, Item, Table, Value};

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_CONFIG_PUBLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
fn fail_next_config_publish_for_test() {
    FAIL_NEXT_CONFIG_PUBLISH.with(|flag| flag.set(true));
}

#[cfg(test)]
fn before_config_publish_for_test() -> Result<()> {
    #[cfg(unix)]
    if FAIL_NEXT_CONFIG_PUBLISH.with(|flag| flag.replace(false)) {
        bail!("injected config publication failure");
    }
    Ok(())
}

/// What writing a given config key does to the RUNNING gateway.
///
/// This is not decoration. Many subsystems call `config::load()` fresh
/// on every use (memory limits, the MCP registry, ACL resolution,
/// checkpoints, cron, plugin loading, theme/keymap), so edits there are
/// live the moment the file lands. Others read the `Arc<VelesConfig>`
/// captured at boot. A settings pane that cannot tell those apart makes
/// the user restart for nothing, or — worse — lets them believe a
/// change took that did not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Re-read on next use: in effect as soon as the file is written.
    Live,
    /// Read when an agent is constructed: new sessions get it, sessions
    /// already open keep the snapshot they started with.
    NewSessions,
    /// Captured at process start: needs `veles gateway restart`.
    RestartGateway,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Live => "live",
            Effect::NewSessions => "new_sessions",
            Effect::RestartGateway => "restart_gateway",
        }
    }

    /// One line the UI can show verbatim next to the control.
    pub fn note(self) -> &'static str {
        match self {
            Effect::Live => "in effect now",
            Effect::NewSessions => "applies to new sessions; open ones keep the old value",
            Effect::RestartGateway => "needs a gateway restart",
        }
    }
}

/// What a completed write did.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EditReport {
    pub path: PathBuf,
    /// Absent when there was no previous file to back up.
    pub backup: Option<PathBuf>,
    pub effect: Effect,
}

pub fn config_path() -> PathBuf {
    crate::config::config_dir().join("config.toml")
}

/// Classify a dotted config path. Longest matching prefix wins, so
/// `agent.guards.max_iterations` resolves through `agent`.
///
/// The Live list is derived from the call sites that re-load: see
/// `memory.rs`, `tools.rs`, `ask.rs`, `checkpoints.rs`, `cronjob.rs`,
/// `tclrt.rs` (`load_plugins`), `theme.rs`, `keymap.rs`. If one of those
/// ever starts caching, its row here has to move — a wrong answer is
/// worse than no answer.
pub fn effect_for(path: &str) -> Effect {
    // Keys whose readers go through `config::live()` (mtime-cached)
    // rather than their section's boot snapshot. Matched on the FULL
    // path and before the section fallback, because the rest of
    // `[sandbox]` — backend, image, mounts — really is wired once at
    // startup. `user_envs::admin_default_is_sandbox` /
    // `local_default_is_sandbox`, `sandboxes::defs` / `binds` and
    // `user_envs::max_concurrent_exec` all re-read per call, so
    // telling a settings pane "restart the gateway" for these would be
    // a lie in the other direction.
    if matches!(
        path,
        "sandbox.admin_default_zone"
            | "sandbox.local_default_zone"
            | "sandbox.envs"
            | "sandbox.assign"
            | "sandbox.max_concurrent_exec"
    ) {
        return Effect::Live;
    }
    let head = path.split('.').next().unwrap_or(path);
    match head {
        // Re-read per use.
        "memory" | "mcp_servers" | "acl" | "checkpoints" | "plugins" | "tui" | "skills"
        | "tool_aliases" | "hooks" => Effect::Live,
        // `[vision]` looks like it belongs with `[model]` below and does
        // not. Its only two readers — `vision::vision_route` (through
        // `call_vision_model`) and `/defaults`' own listing — each do a
        // fresh `config::load()` per call, so the route is in force the
        // moment the file lands. Classified NewSessions, it sent an
        // operator who had just repaired a broken vision route off to
        // start a session they did not need. If a reader ever starts
        // taking this off the boot `Arc`, this row moves back.
        "vision" => Effect::Live,
        // Held by the boot Arc, but a NEW session is built on the live
        // file rather than that snapshot: `GatewayCore::session_cfg`
        // re-reads through `config::live()` for every agent it mints
        // (and `/new` goes the same way through `frontends::fork`). So
        // "new sessions" means new sessions, not new processes — which
        // is what `/defaults` tells the operator when it writes here.
        // The desktop's POST /api/config/reload is the other reader.
        "model" | "model_aliases" | "custom_providers" | "fallback_providers" | "failover"
        | "mechanisms" | "agent" | "approvals" | "a2a_agents" | "ui" => Effect::NewSessions,
        // Adapters, listeners and secrets are wired once at startup.
        // `sandbox` too: SandboxSection::current() caches the section in
        // a process-wide OnceLock, so a live process keeps the value it
        // first saw — new sessions do NOT re-read it (restart_gateway,
        // not the default new_sessions).
        "platforms" | "security" | "display" | "sandbox" => Effect::RestartGateway,
        _ => Effect::NewSessions,
    }
}

/// Parse the current config.toml, or an empty document when there is
/// none yet (a first write must not require the file to pre-exist).
pub fn read_doc() -> Result<DocumentMut> {
    let path = config_path();
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    raw.parse::<DocumentMut>()
        .with_context(|| format!("{} is not valid TOML", path.display()))
}

/// Apply `mutate` to config.toml and persist it.
///
/// `path_hint` is the dotted key the caller is changing; it only picks
/// the `Effect` reported back. Nothing is written when `mutate` fails or
/// when the result would not deserialize.
pub fn edit<F>(path_hint: &str, mutate: F) -> Result<EditReport>
where
    F: FnOnce(&mut DocumentMut) -> Result<()>,
{
    let path = config_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = read_doc()?;
    mutate(&mut doc)?;
    let rendered = doc.to_string();

    // Refuse to write something the agent could not read back.
    if let Err(e) = toml::from_str::<crate::config::VelesConfig>(&rendered) {
        bail!("the edit would make config.toml unreadable: {e}");
    }

    let backup = write_with_backup(&path, &existing, &rendered)?;
    Ok(EditReport {
        path,
        backup,
        effect: effect_for(path_hint),
    })
}

/// Write `rendered` over config.toml at `path`, whose current text is
/// `existing` ("" when there is no file): back the old text up first,
/// with the old file's mode, and create a new file owner-only.
///
/// THE one write rule for config.toml. `edit` uses it, and so does the
/// setup wizard (`setup::apply`), which used to carry its own copy of
/// the backup and skipped the owner-only step — its first config.toml
/// was born at the umask, 0644, beside a `.bak` rule that had already
/// learned that lesson here.
pub fn write_with_backup(
    path: &std::path::Path,
    existing: &str,
    rendered: &str,
) -> Result<Option<PathBuf>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Preserve an operator's config.toml symlink: publish beside its
    // target, not over the link. The temporary and target share a
    // filesystem, so a successful rename is the only publication point.
    let destination = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("resolving config symlink {}", path.display()))?,
        _ => path.to_path_buf(),
    };
    let parent = destination
        .parent()
        .with_context(|| format!("config path {} has no parent", destination.display()))?;
    let name = destination
        .file_name()
        .context("config destination has no filename")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.edit-{}.tmp", std::process::id()));
    let backup = if existing.is_empty() {
        None
    } else {
        let bak = path.with_extension("toml.bak");
        // write_private applies the Windows owner-only DACL before
        // truncating or exposing old/new secret bytes. On Unix it also
        // tightens a pre-existing broad backup before writing.
        crate::platform::write_private(&bak, existing.as_bytes())
            .with_context(|| format!("writing private backup {}", bak.display()))?;
        // Unix mirrors the original target's permissions, not the umask's.
        mirror_mode(&destination, &bak);
        Some(bak)
    };
    let publication = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        crate::platform::private_open_options(&mut opts);
        let mut file = opts
            .open(&temporary)
            .with_context(|| format!("creating private staged config {}", temporary.display()))?;
        // On Unix create_new + mode(0600) makes the first bytes private.
        // Windows needs its explicit DACL too; never inherit a broad ACL.
        #[cfg(windows)]
        crate::platform::restrict_file(&temporary)
            .with_context(|| format!("restricting staged config {}", temporary.display()))?;
        #[cfg(unix)]
        if destination.exists() {
            copy_mode(&destination, &temporary)
                .with_context(|| format!("copying config mode to {}", temporary.display()))?;
        }
        use std::io::Write;
        file.write_all(rendered.as_bytes())
            .with_context(|| format!("writing staged config {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("flushing staged config {}", temporary.display()))?;
        drop(file);
        #[cfg(test)]
        before_config_publish_for_test()?;
        crate::platform::replace_file(&temporary, &destination)
            .with_context(|| format!("publishing {}", destination.display()))?;
        Ok(())
    })();
    if publication.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    publication?;
    Ok(backup)
}

/// Copy `src`'s permission bits onto `dst`.
///
/// Best effort by design: a failure here must not lose the edit that has
/// already been written, and on a filesystem with no mode bits there is
/// nothing to mirror. It is logged rather than swallowed, because a
/// backup that silently stayed world-readable is precisely the thing
/// this exists to stop.
#[cfg(unix)]
pub(crate) fn mirror_mode(src: &std::path::Path, dst: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = match std::fs::metadata(src) {
        Ok(m) => m.permissions().mode() & 0o7777,
        Err(e) => {
            tracing::warn!("config backup: cannot read {}'s mode: {e}", src.display());
            return;
        }
    };
    if let Err(e) = std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode)) {
        tracing::warn!(
            "config backup {} kept the umask mode instead of {mode:o}: {e}",
            dst.display()
        );
    }
}

#[cfg(not(unix))]
pub(crate) fn mirror_mode(_src: &std::path::Path, _dst: &std::path::Path) {}

/// An atomic replacement must carry the target's Unix permission bits.
/// Windows uses the owner-only DACL applied to the staged file above;
/// its inherited ACL is not copied onto an unrelated destination.
#[cfg(unix)]
fn copy_mode(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(src)?.permissions().mode() & 0o7777;
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode))
}

/// Owner-only helper for other config writers that stage a new file.
/// The atomic writer above uses platform::private_open_options instead.
#[cfg(unix)]
pub(crate) fn restrict_to_owner(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("new config {} left at the umask mode: {e}", path.display());
    }
}

#[cfg(not(unix))]
pub(crate) fn restrict_to_owner(_path: &std::path::Path) {}

// ── path-addressed document surgery ─────────────────────────────────
//
// Paths are dotted and addressed as slices: `["agent", "guards",
// "max_iterations"]`. Missing intermediate tables are created; nothing
// else in the document is touched.

/// Set one key, creating parent tables as needed.
pub fn set_value(doc: &mut DocumentMut, path: &[&str], value: Value) -> Result<()> {
    set_item(doc.as_item_mut(), path, Item::Value(value))
}

/// Remove one key. A path that does not exist is not an error — the
/// caller asked for it to be absent, and it is.
pub fn unset(doc: &mut DocumentMut, path: &[&str]) -> Result<()> {
    let (last, parents) = path.split_last().context("unset needs a non-empty path")?;
    let Some(item) = get_item_mut(doc.as_item_mut(), parents) else {
        return Ok(());
    };
    if let Some(t) = item.as_table_like_mut() {
        t.remove(last);
    }
    Ok(())
}

/// Add a string to an array-valued key (creating it), idempotently.
pub fn array_add(doc: &mut DocumentMut, path: &[&str], value: &str) -> Result<()> {
    let mut arr = string_array(doc, path);
    if arr.iter().any(|v| v == value) {
        return Ok(());
    }
    arr.push(value.to_string());
    set_string_array(doc, path, &arr)
}

/// Remove a string from an array-valued key. Absent is fine.
pub fn array_remove(doc: &mut DocumentMut, path: &[&str], value: &str) -> Result<()> {
    let mut arr = string_array(doc, path);
    let before = arr.len();
    arr.retain(|v| v != value);
    if arr.len() == before {
        return Ok(());
    }
    set_string_array(doc, path, &arr)
}

/// Read an array of strings, empty when absent or the wrong shape.
pub fn string_array(doc: &DocumentMut, path: &[&str]) -> Vec<String> {
    let Some(item) = get_item(doc.as_item(), path) else {
        return Vec::new();
    };
    let Some(arr) = item.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| v.as_str())
        .map(str::to_string)
        .collect()
}

pub fn set_string_array(doc: &mut DocumentMut, path: &[&str], values: &[String]) -> Result<()> {
    let mut arr = toml_edit::Array::new();
    for v in values {
        arr.push(v.as_str());
    }
    set_value(doc, path, Value::Array(arr))
}

/// Is a key present at all? Distinguishes "the user set it to the
/// default" from "the user never mentioned it", which is what a
/// three-state on/off/default control needs.
pub fn has(doc: &DocumentMut, path: &[&str]) -> bool {
    get_item(doc.as_item(), path)
        .map(|i| !i.is_none())
        .unwrap_or(false)
}

pub fn set_item(item: &mut Item, path: &[&str], value: Item) -> Result<()> {
    let (head, rest) = path.split_first().context("empty config path")?;
    if item.is_none() {
        *item = Item::Table(Table::new());
    }
    let table = item
        .as_table_like_mut()
        .with_context(|| format!("cannot descend into `{head}`: parent is not a table"))?;
    if rest.is_empty() {
        table.insert(head, value);
        return Ok(());
    }
    if table.get(head).map(|i| i.is_none()).unwrap_or(true) {
        table.insert(head, Item::Table(Table::new()));
    }
    let next = table
        .get_mut(head)
        .with_context(|| format!("`{head}` vanished mid-write"))?;
    set_item(next, rest, value)
}

fn get_item<'a>(item: &'a Item, path: &[&str]) -> Option<&'a Item> {
    let mut cur = item;
    for key in path {
        cur = cur.as_table_like()?.get(key)?;
    }
    Some(cur)
}

pub fn get_item_mut<'a>(item: &'a mut Item, path: &[&str]) -> Option<&'a mut Item> {
    let mut cur = item;
    for key in path {
        cur = cur.as_table_like_mut()?.get_mut(key)?;
    }
    Some(cur)
}

/// Replace whole top-level keys from a TOML fragment the UI generated.
///
/// This is what the failover blueprint's "Save globally" needs: the
/// editor emits `fallback_providers = [...]` plus a run of
/// `[[failover]]` blocks, and those two keys — and only those two —
/// must take over from whatever the file said. A key absent from the
/// fragment is REMOVED, so clearing every fallback in the editor
/// actually clears them in the file.
///
/// Position is preserved for keys that already exist (toml_edit keeps a
/// replaced key where it was), and comments inside the fragment come
/// along with it.
pub fn replace_sections(doc: &mut DocumentMut, block: &str, keys: &[&str]) -> Result<()> {
    let incoming = block
        .parse::<DocumentMut>()
        .context("the supplied block is not valid TOML")?;
    for key in keys {
        // The comment lines above the old key are the USER'S — often the
        // file header, since these keys sit near the top — and they are
        // stored as that key's leading decor. Replacing the item would
        // take them with it, so carry them across. The generated block's
        // own boilerplate comment only survives where the file had none.
        let kept = leading_decor(doc.as_table(), key);
        match incoming.as_table().get(key) {
            Some(item) => {
                doc.insert(key, item.clone());
                if let Some(prefix) = kept {
                    set_leading_decor(doc.as_table_mut(), key, &prefix);
                }
            }
            None => {
                doc.remove(key);
            }
        }
    }
    Ok(())
}

/// Where a key's leading comments actually live depends on its shape:
/// on the table for `[key]`, on the FIRST table for `[[key]]`, and on
/// the key itself for `key = value`.
fn leading_decor(table: &Table, key: &str) -> Option<String> {
    let from_item = match table.get(key) {
        Some(Item::Table(t)) => t.decor().prefix(),
        Some(Item::ArrayOfTables(a)) => a.get(0).and_then(|t| t.decor().prefix()),
        _ => None,
    };
    let raw = from_item.or_else(|| {
        table
            .key(key)
            .map(|k| k.leaf_decor())
            .and_then(|d| d.prefix())
    });
    let text = raw.and_then(|r| r.as_str())?;
    (!text.trim().is_empty()).then(|| text.to_string())
}

fn set_leading_decor(table: &mut Table, key: &str, prefix: &str) {
    match table.get_mut(key) {
        Some(Item::Table(t)) => {
            t.decor_mut().set_prefix(prefix);
        }
        Some(Item::ArrayOfTables(a)) => {
            if let Some(t) = a.get_mut(0) {
                t.decor_mut().set_prefix(prefix);
            }
        }
        _ => {
            if let Some(mut k) = table.key_mut(key) {
                k.leaf_decor_mut().set_prefix(prefix);
            }
        }
    }
}

// ── JSON ⇄ TOML at the edge ─────────────────────────────────────────
//
// The desktop speaks JSON, config.toml is TOML. Both are values, but
// TOML has no null: an explicit JSON `null` means "unset this key",
// which is a different request from "set it to something", so the
// conversion refuses it and the route handles it as a removal.

/// Convert a JSON value into a TOML one. `null` is rejected on purpose
/// — see above.
pub fn json_to_toml(v: &serde_json::Value) -> Result<Value> {
    Ok(match v {
        serde_json::Value::Null => bail!("null is not a TOML value — send it as an unset instead"),
        serde_json::Value::Bool(b) => (*b).into(),
        serde_json::Value::String(s) => s.as_str().into(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into()
            } else if let Some(f) = n.as_f64() {
                f.into()
            } else {
                bail!("number out of range for TOML: {n}")
            }
        }
        serde_json::Value::Array(items) => {
            let mut arr = toml_edit::Array::new();
            for item in items {
                arr.push(json_to_toml(item)?);
            }
            Value::Array(arr)
        }
        serde_json::Value::Object(map) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, val) in map {
                t.insert(k, json_to_toml(val)?);
            }
            Value::InlineTable(t)
        }
    })
}

/// Text typed into a field that nothing typed: a TOML literal when the
/// TOML library reads it as one (`5`, `true`, `"x"`, `["a"]`), else the
/// text itself as a string.
pub fn literal_or_string(text: &str) -> Value {
    text.trim()
        .parse::<Value>()
        .unwrap_or_else(|_| Value::from(text.trim()))
}

/// Read one path back as JSON. `None` when the key is not present —
/// which a three-state control needs to tell "set to the default" from
/// "never mentioned".
pub fn get_json(doc: &DocumentMut, path: &[&str]) -> Option<serde_json::Value> {
    get_json_item(doc.as_item(), path)
}

/// Read a value below an existing item, including one table inside an
/// array of tables. The installer uses this to edit a named instance
/// without rebuilding the array or turning it into inline tables.
pub fn get_json_item(item: &Item, path: &[&str]) -> Option<serde_json::Value> {
    let item = get_item(item, path)?;
    if item.is_none() {
        return None;
    }
    // toml → serde_json via the TOML value model, which both sides
    // already agree on; hand-walking the tree here would be a third
    // encoder to keep in step with the other two.
    let parsed: toml::Value = item.to_string().trim().parse().ok().or_else(|| {
        // A bare scalar is not a document; wrap it so it parses.
        format!("v = {}", item.to_string().trim())
            .parse::<toml::Value>()
            .ok()
            .and_then(|t| t.get("v").cloned())
    })?;
    serde_json::to_value(parsed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> DocumentMut {
        s.parse::<DocumentMut>().expect("fixture must parse")
    }

    /// The invariant `splice_failover` existed for, now enforced on the
    /// general editor: replacing the failover sections leaves every
    /// other line — and every comment, inline ones included — untouched.
    ///
    /// The fixture puts `fallback_providers` at ROOT level, where it
    /// actually lives (`VelesConfig::fallback_providers`) and where a
    /// real file has it — see `~/.config/veles/config.toml`, which spells it
    /// `[[fallback_providers]]` before any other table. The ancestor's
    /// fixture wrote it AFTER `[model]`, i.e. as a key of `[model]`,
    /// which serde ignores entirely; a line walker could not tell the
    /// difference, and a TOML-aware editor should not pretend to.
    #[test]
    fn replacing_sections_leaves_everything_else_byte_identical() {
        let mut d = doc("\
# my hand-written config, with comments I care about
fallback_providers = [
  { provider = \"old1\" },
  { provider = \"old2\" },
]

[model]
provider = \"openrouter\"   # inline comment
default = \"claude\"

[[failover]]
provider = \"old1\"
mode = \"auto\"

[[failover]]
provider = \"*\"
mode = \"ask\"

[memory]
memory_char_limit = 44000
");
        let block = "fallback_providers = [\n  { provider = \"new\" },\n]\n\n[[failover]]\nprovider = \"new\"\nmode = \"stop\"\n";
        replace_sections(&mut d, block, &["fallback_providers", "failover"]).unwrap();
        let out = d.to_string();

        assert!(
            out.contains("# my hand-written config, with comments I care about"),
            "{out}"
        );
        assert!(
            out.contains("provider = \"openrouter\"   # inline comment"),
            "{out}"
        );
        assert!(out.contains("[memory]"), "{out}");
        assert!(out.contains("memory_char_limit = 44000"), "{out}");

        assert!(!out.contains("old1"), "old fallback entries removed: {out}");
        assert!(!out.contains("old2"), "{out}");
        assert!(!out.contains("mode = \"auto\""), "old rules removed: {out}");

        assert!(out.contains("{ provider = \"new\" }"), "{out}");
        assert!(out.contains("mode = \"stop\""), "{out}");
        assert_eq!(out.matches("fallback_providers").count(), 1, "{out}");
        assert_eq!(out.matches("[[failover]]").count(), 1, "{out}");

        let parsed: toml::Value = toml::from_str(&out).expect("result must parse");
        assert!(parsed.get("memory").is_some(), "{out}");
    }

    /// A `fallback_providers` written under a table header belongs to
    /// THAT table, so serde never read it and this editor does not
    /// touch it. Documented rather than "fixed": silently deleting a
    /// key because it looks like one we manage is how an editor eats
    /// something a user meant to keep.
    #[test]
    fn a_misplaced_key_under_another_table_is_left_alone() {
        let mut d =
            doc("[model]\nprovider = \"p\"\nfallback_providers = [{ provider = \"inert\" }]\n");
        replace_sections(
            &mut d,
            "fallback_providers = [{ provider = \"real\" }]\n",
            &["fallback_providers"],
        )
        .unwrap();
        let out = d.to_string();
        assert!(
            out.contains("inert"),
            "the inert copy stays where it was: {out}"
        );
        let parsed: crate::config::VelesConfig = toml::from_str(&out).unwrap();
        assert_eq!(
            parsed.fallback_providers.len(),
            1,
            "only the root-level one is read"
        );
    }

    #[test]
    fn replacing_appends_when_there_is_nothing_to_replace() {
        let mut d = doc("[model]\nprovider = \"p\"\n");
        replace_sections(
            &mut d,
            "[[failover]]\nmode = \"stop\"\n",
            &["fallback_providers", "failover"],
        )
        .unwrap();
        let out = d.to_string();
        assert!(out.contains("provider = \"p\""), "{out}");
        assert!(out.contains("[[failover]]"), "{out}");
        let _: toml::Value = toml::from_str(&out).expect("must parse");
    }

    /// A `[[failover]]` run used to be delimited by hand ("until the
    /// next table header"), which is exactly the kind of thing a line
    /// walker gets wrong. The table that follows must survive.
    #[test]
    fn replacing_does_not_swallow_the_next_table() {
        let mut d = doc("[[failover]]\nmode = \"auto\"\n[agent]\nuser_agent = \"keep me\"\n");
        replace_sections(&mut d, "[[failover]]\nmode = \"stop\"\n", &["failover"]).unwrap();
        let out = d.to_string();
        assert!(out.contains("[agent]"), "the next table survives: {out}");
        assert!(out.contains("user_agent = \"keep me\""), "{out}");
        assert!(!out.contains("mode = \"auto\""), "{out}");
    }

    /// A key absent from the fragment means "the user removed it".
    #[test]
    fn a_key_missing_from_the_block_is_removed() {
        let mut d =
            doc("fallback_providers = [{ provider = \"a\" }]\n[memory]\nmemory_char_limit = 1\n");
        replace_sections(
            &mut d,
            "[[failover]]\nmode = \"stop\"\n",
            &["fallback_providers", "failover"],
        )
        .unwrap();
        let out = d.to_string();
        assert!(!out.contains("fallback_providers"), "{out}");
        assert!(out.contains("memory_char_limit = 1"), "{out}");
    }

    #[test]
    fn set_value_creates_missing_parent_tables_and_keeps_comments() {
        let mut d = doc("# keep me\n[model]\nprovider = \"p\"  # and me\n");
        set_value(&mut d, &["agent", "guards", "max_iterations"], 42.into()).unwrap();
        let out = d.to_string();
        assert!(out.contains("# keep me"), "{out}");
        assert!(out.contains("provider = \"p\"  # and me"), "{out}");
        let parsed: crate::config::VelesConfig = toml::from_str(&out).expect("{out}");
        assert_eq!(parsed.agent.guards.max_iterations, 42, "{out}");
    }

    #[test]
    fn set_value_overwrites_in_place_without_duplicating() {
        let mut d = doc("[agent.guards]\nmax_iterations = 500\nguardrail_warn_at = 3\n");
        set_value(&mut d, &["agent", "guards", "max_iterations"], 7.into()).unwrap();
        let out = d.to_string();
        assert_eq!(out.matches("max_iterations").count(), 1, "{out}");
        assert!(
            out.contains("guardrail_warn_at = 3"),
            "sibling untouched: {out}"
        );
        let parsed: crate::config::VelesConfig = toml::from_str(&out).unwrap();
        assert_eq!(parsed.agent.guards.max_iterations, 7);
    }

    #[test]
    fn array_add_and_remove_are_idempotent() {
        let mut d = doc("");
        array_add(&mut d, &["plugins", "disabled"], "noisy").unwrap();
        array_add(&mut d, &["plugins", "disabled"], "noisy").unwrap();
        assert_eq!(
            string_array(&d, &["plugins", "disabled"]),
            vec!["noisy".to_string()]
        );

        array_add(&mut d, &["plugins", "disabled"], "other").unwrap();
        array_remove(&mut d, &["plugins", "disabled"], "noisy").unwrap();
        array_remove(&mut d, &["plugins", "disabled"], "noisy").unwrap();
        assert_eq!(
            string_array(&d, &["plugins", "disabled"]),
            vec!["other".to_string()]
        );

        let parsed: crate::config::VelesConfig = toml::from_str(&d.to_string()).unwrap();
        assert_eq!(parsed.plugins.disabled, vec!["other".to_string()]);
    }

    #[test]
    fn unset_removes_and_tolerates_absence() {
        let mut d = doc("[agent]\nuser_agent = \"x\"\nreasoning_effort = \"high\"\n");
        unset(&mut d, &["agent", "user_agent"]).unwrap();
        unset(&mut d, &["agent", "user_agent"]).unwrap();
        unset(&mut d, &["nothing", "here"]).unwrap();
        let out = d.to_string();
        assert!(!out.contains("user_agent"), "{out}");
        assert!(out.contains("reasoning_effort"), "{out}");
    }

    #[test]
    fn has_distinguishes_unset_from_set_to_the_default() {
        let mut d = doc("");
        assert!(!has(&d, &["agent", "guards", "verification_nudge"]));
        set_value(
            &mut d,
            &["agent", "guards", "verification_nudge"],
            true.into(),
        )
        .unwrap();
        assert!(has(&d, &["agent", "guards", "verification_nudge"]));
    }

    /// The desktop-only `[ui]` block is not modelled by VelesConfig and
    /// is read straight off the file by the Tcl side. An edit elsewhere
    /// must not disturb it — which is only true because nothing here
    /// ever re-serializes from the struct.
    #[test]
    fn unmodelled_sections_survive_an_edit() {
        let mut d =
            doc("[ui]\ncoach = true\nfont_scale = 1.25\n\n[memory]\nmemory_char_limit = 1\n");
        set_value(&mut d, &["memory", "memory_char_limit"], 2.into()).unwrap();
        let out = d.to_string();
        assert!(out.contains("[ui]"), "{out}");
        assert!(out.contains("coach = true"), "{out}");
        assert!(out.contains("font_scale = 1.25"), "{out}");
        let _: crate::config::VelesConfig = toml::from_str(&out).expect("{out}");
    }

    #[test]
    fn effects_are_classified_by_head_segment() {
        assert_eq!(effect_for("plugins.disabled"), Effect::Live);
        assert_eq!(effect_for("memory"), Effect::Live);
        assert_eq!(
            effect_for("agent.guards.max_iterations"),
            Effect::NewSessions
        );
        assert_eq!(effect_for("model_aliases.fast"), Effect::NewSessions);
        assert_eq!(effect_for("platforms.irc.nickname"), Effect::RestartGateway);
        // …with per-key exceptions where the READER re-reads. The rest
        // of `[sandbox]` is wired once at startup; these five are not.
        assert_eq!(effect_for("sandbox.backend"), Effect::RestartGateway);
        assert_eq!(effect_for("sandbox.admin_default_zone"), Effect::Live);
        assert_eq!(effect_for("sandbox.local_default_zone"), Effect::Live);
        assert_eq!(effect_for("sandbox.envs"), Effect::Live);
        assert_eq!(effect_for("sandbox.assign"), Effect::Live);
        assert_eq!(effect_for("sandbox.max_concurrent_exec"), Effect::Live);
        // The network posture is NOT live: `EnvCfg::current()` reads the
        // process-cached section, and a container keeps whatever
        // namespace it was created in regardless. Saying `live` here
        // would be the exact lie `Effect` exists to prevent — an
        // operator tightening the fence and being told it is already in
        // force. After the restart, `ensure` rebuilds any box whose
        // namespace no longer matches (`net_mode_matches`), so the
        // container side needs no separate step.
        assert_eq!(
            effect_for("sandbox.user_env_network"),
            Effect::RestartGateway
        );
        // The screen route is read per call off the session's config,
        // so it moves with the section it lives in.
        assert_eq!(effect_for("approvals.screen_provider"), Effect::NewSessions);
        assert_eq!(effect_for("mechanisms.platforms"), Effect::NewSessions);
        // `[vision]` has exactly two readers — `vision::vision_route`
        // through `call_vision_model`, and `/defaults`' own listing —
        // and BOTH do a fresh `config::load()` per call. So the route is
        // in force the moment the file lands, and saying "new sessions"
        // sent an operator who had just fixed a broken vision route off
        // to start a session they did not need. Found by doing exactly
        // that, on 2026-09-12, with `/defaults vision`.
        assert_eq!(effect_for("vision"), Effect::Live);
        assert_eq!(effect_for("vision.model"), Effect::Live);
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupted_publication_leaves_the_old_config_and_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        fail_next_config_publish_for_test();
        let error = write_with_backup(&path, "old\n", "new\n").unwrap_err();
        assert!(error.to_string().contains("injected"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!temp.path().join("config.toml.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_existing_config_keeps_its_mode_and_its_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("target.toml");
        let path = temp.path().join("config.toml");
        std::fs::write(&target, "").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        write_with_backup(&path, "", "new\n").unwrap();
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    /// The backup must never be readable by people who cannot read the
    /// file it copies.
    ///
    /// `config.toml` on this box is 0600 and holds a network's SASL
    /// password (the multi-network IRC path cannot take credentials from
    /// .env, so they live in the TOML). `std::fs::write` on a path that
    /// does not exist yet creates it at the process umask — 0644 here —
    /// so every edit published a world-readable copy of a file that was
    /// deliberately locked down. The live `config.toml.bak` on this
    /// machine was 0644 when this was found.
    ///
    /// Note what does NOT have the bug: rewriting `config.toml` itself
    /// truncates an existing inode and keeps its mode. Only the backup
    /// is created fresh, which is why only the backup leaked.
    #[cfg(unix)]
    #[test]
    fn the_backup_is_no_more_readable_than_the_file_it_copies() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::testenv::lock();
        let home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VELES_HOME", home.path());
        let dir = crate::config::config_dir();
        std::fs::create_dir_all(&dir).expect("config dir");
        let path = dir.join("config.toml");

        std::fs::write(&path, "[model]\nprovider = \"p\"\n").expect("seed");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let report = edit("model.default", |d| {
            set_value(d, &["model", "default"], "m".into())
        })
        .expect("edit");

        let bak = report.backup.expect("a backup was made");
        let mode = std::fs::metadata(&bak).expect("bak").permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the backup of a 0600 config must be 0600, not {mode:o} — it holds the same secrets"
        );
        let live = std::fs::metadata(&path).expect("cfg").permissions().mode() & 0o777;
        assert_eq!(live, 0o600, "and the rewritten config keeps its own mode");
    }

    /// A config created from nothing is 0600, because it is a file that
    /// holds credentials by design — provider keys, a SASL password —
    /// and inheriting a 0644 umask for it is the same defect as the
    /// backup above, one step earlier.
    #[cfg(unix)]
    #[test]
    fn a_config_written_from_nothing_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::testenv::lock();
        let home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VELES_HOME", home.path());
        let path = config_path();
        assert!(!path.exists(), "this test needs to create it");

        let report = edit("model.default", |d| {
            set_value(d, &["model", "default"], "m".into())
        })
        .expect("edit");
        assert!(report.backup.is_none(), "nothing to back up");

        let mode = std::fs::metadata(&path).expect("cfg").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got {mode:o}");
    }
}
