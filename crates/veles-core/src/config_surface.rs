//! Every option the binary reads, derived from the structs that read it.
//!
//! `config.toml` has no schema a human can read: the truth is ~90 leaves
//! in 34 tables spread over `config.rs`, and an installation that never
//! wrote `[acl]` or `[sandbox]` runs on whatever those defaults are with
//! no way to ask what they were. That is how a box ends up fenced
//! `native` and nobody can say why. This module turns the surface into a
//! value you can print and merge:
//!
//! * [`knobs`] walks `VelesConfig::default()` through serde, so it
//!   cannot rot when a field is added. An `Option` field comes back with
//!   no default: unset IS its default, and writing `""` for one would
//!   change behaviour.
//! * [`defaults_text`] renders a complete config at defaults.
//! * [`fill`] documents what is missing in an existing document, as
//!   comments: `# key = value  # default`. It never pins a default, so a
//!   changed code default reaches every install (USER 2026-10-04: "yes
//!   but document the defaults in the file as comments"). Edits are made
//!   in place, so comments, key order and the desktop-only `[ui]` block
//!   all survive.
//! * [`tidy`] turns a live line that only restates its default into that
//!   same comment — what an older fill, which wrote defaults as values,
//!   left behind.
//!
//! Three limits, each deliberate and each pinned by a test:
//!
//! 1. The free-form roots ([`free_form`]) are read ad hoc from a
//!    `serde_yaml::Value`, so there is no struct to walk. They are
//!    neither filled nor called dead — a false "dead" verdict on a key
//!    the IRC adapter really reads is worse than no verdict.
//! 2. Arrays of tables are documented as comments, never emitted as
//!    `x = []`: TOML then refuses to let the user expand that key into
//!    `[[x]]` later, so filling one would trap the next edit. Which
//!    paths those are is *derived*, not listed — probe-parse `[[path]]`
//!    and read the error kind.
//! 3. For the same reason the dead-key walk stops at arrays: auditing
//!    `[[custom_providers]]` entries needs an element schema, and
//!    guessing one produces false verdicts.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{bail, Context, Result};
use toml_edit::{DocumentMut, Item, Table, Value};

use crate::config::VelesConfig;

// The `Knob`/`Shape` type moved to `veles_types::knob` (Phase 3, D2): a
// process plugin's `--describe` carries the same knobs, so the type is
// shared. The FILL logic below (which reads `VelesConfig` and writes
// TOML) stays here. Re-exported so every existing `config_surface::Knob`
// path still compiles.
pub use veles_types::knob::{Knob, Shape};

/// Paths whose type is an open map (`HashMap<String, _>`): the names
/// inside them are the user's, so nothing there can be judged dead, and
/// there is nothing to fill either — the surface is the map's *value*
/// type, not its keys.
///
/// Derived, not listed. An empty map is the only thing that serializes to
/// an empty object here: a struct's unset `Option` fields arrive as nulls,
/// so `{}` means "keys I have never seen".
pub fn open_maps() -> Vec<String> {
    let value = serde_json::to_value(VelesConfig::default()).expect("VelesConfig serializes");
    let mut out = Vec::new();
    collect_open_maps(&value, String::new(), &mut out);
    out.sort();
    out
}

fn collect_open_maps(value: &serde_json::Value, prefix: String, out: &mut Vec<String>) {
    let map = match value {
        serde_json::Value::Object(m) => m,
        _ => return,
    };
    if map.is_empty() {
        if !prefix.is_empty() {
            out.push(prefix);
        }
        return;
    }
    for (key, child) in map {
        collect_open_maps(child, join(&prefix, key), out);
    }
}

/// Is `path` inside an open map, where the keys belong to the user?
fn under_open_map(path: &str, open: &[String]) -> bool {
    open.iter()
        .any(|root| path == root || path.starts_with(&format!("{root}.")))
}

/// Roots read ad hoc rather than through a struct: no surface to derive,
/// so no verdicts. `[ui]` is the desktop's own (the Tcl side reads it
/// directly); the rest are connector and registry blobs.
pub fn free_form() -> &'static [&'static str] {
    &["platforms", "display", "ui", "mcp_servers"]
}

/// Every typed option with its default, sorted by path.
///
/// The platform blocks core reads itself are NOT included: a caller that
/// has not supplied the connector surfaces would then get dead-key
/// verdicts inside `platforms.*` that only look true. `blob_surface` in
/// the CLI assembles the complete set.
pub fn knobs() -> Vec<Knob> {
    let value = serde_json::to_value(VelesConfig::default()).expect("VelesConfig serializes");
    let mut out = Vec::new();
    walk_json(&value, String::new(), &mut out);
    out.retain(|k| !under_free_form(&k.path));
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The `[platforms.*]` blocks core reads without a connector crate:
/// `enabled` plus an `extra.port` that an env var beats.
///
/// Documented rather than written, like every blob option: an `IRC_*`-style
/// var or a verbosity profile usually decides these, and a fill that wrote
/// the default would create a second source of truth that quietly loses.
pub fn platform_surface() -> Vec<Knob> {
    vec![
        Knob::blob_documented(
            "platforms.a2a.enabled",
            "false",
            "agent-to-agent inbound server",
        ),
        Knob::blob_env("platforms.a2a.extra.port", Some("9900"), "A2A_PORT", None),
        // Per machine (configsync::NODE_KEYS); pool join writes it. Unset
        // binds 127.0.0.1, and a host without a token never widens it.
        Knob::blob_env(
            "platforms.a2a.extra.host",
            None,
            "A2A_HOST",
            Some("this node's bind address; A2A_HOST beats it; needs a token to widen"),
        ),
        Knob::blob_documented(
            "platforms.desktop.enabled",
            "true",
            "desktop API: loopback bind, bearer token",
        ),
        // Not `.env`-settable: this one is read with a plain
        // `std::env::var`, so it takes the process environment only.
        Knob::blob_env(
            "platforms.desktop.extra.port",
            Some("8642"),
            "VELES_DESKTOP_PORT",
            None,
        ),
        // HERMES' OWN SPELLING, now live — and HERE rather than in the
        // telegram crate, because the gateway is what reads it
        // (`owner_home_chat`), not the adapter. veles-telegram's own
        // surface test is right to refuse a key its reader never looks
        // at; this list is for exactly the `[platforms.*]` keys core
        // reads on its own.
        //
        // It had been inert since the hermes migration — the doc on
        // `walk_dead` below uses this very table as its worked example
        // of a block that survives an audit as dead config — which is
        // why the owner's telegram id sat in ~/.hermes/config.yaml and
        // nowhere veles could read it.
        Knob::blob_documented(
            "platforms.telegram.home_channel.chat_id",
            "unset",
            "the owner's home chat — a DM, or a private group with the bot (the ops chat): [notify] ask_relay's destination, where cron output with deliver=home goes; a plain reply there answers the waiting question and starts no turn, a turn there needs an @mention as in any group",
        ),
        // The home table (`home::from_config`) reads every platform's
        // `home_channel.chat_id`, so IRC's is read too: cron output with
        // deliver=home goes there on that network. Listed here for the
        // reason the telegram one is: core reads it, and the audit must
        // not call a read key dead.
        Knob::blob_documented(
            "platforms.irc.home_channel.chat_id",
            "unset",
            "the IRC home channel (the ops channel): where cron output with deliver=home goes on this network; relayed questions are Telegram's",
        ),
    ]
}

fn under_free_form(path: &str) -> bool {
    let root = path.split('.').next().unwrap_or(path);
    free_form().contains(&root)
}

/// Does a listed option reach `path`?
///
/// True when `path` is a surface knob, sits inside one, or is a container
/// one lives under — which is what lets the audit judge `platforms.irc`
/// while leaving `[ui]` alone.
fn covered(path: &str, surface: &HashSet<&str>) -> bool {
    surface.iter().any(|k| {
        *k == path || k.starts_with(&format!("{path}.")) || path.starts_with(&format!("{k}."))
    })
}

fn walk_json(value: &serde_json::Value, prefix: String, out: &mut Vec<Knob>) {
    let map = match value {
        serde_json::Value::Object(m) => m,
        other => {
            if prefix.is_empty() {
                return;
            }
            let shape = match other {
                serde_json::Value::Null => Shape::Unset,
                serde_json::Value::Array(items) => {
                    if items.is_empty() && !is_scalar_array(&prefix) {
                        Shape::TableArray
                    } else {
                        Shape::ScalarArray
                    }
                }
                _ => Shape::Value,
            };
            out.push(Knob {
                path: prefix,
                default: literal(other),
                shape,
                note: None,
                env: None,
            });
            return;
        }
    };
    for (key, child) in map {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        walk_json(child, path, out);
    }
}

/// Render a JSON leaf as a TOML literal, or `None` when it is null.
///
/// Delegates to [`crate::configedit::json_to_toml`] rather than encoding
/// again — a third encoder is a third thing to keep in step, and that one
/// already refuses null on purpose.
fn literal(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::Object(_) => None,
        other => crate::configedit::json_to_toml(other)
            .ok()
            .map(|v| v.to_string()),
    }
}

/// Does `path` hold scalars? Derived, not listed: hand the deserializer a
/// one-element array and read the answer.
///
/// Classifying by error *text* instead would misread the lenient fields —
/// `approvals.mode` accepts a bool or a string and complains in its own
/// words, so "did it say `invalid type`" is not a question about the
/// element type at all.
fn is_scalar_array(path: &str) -> bool {
    toml::from_str::<VelesConfig>(&format!("{path} = [\"x\"]\n")).is_ok()
}

// ---------------------------------------------------------------- order

/// Section order and per-section key order, taken from the config's own
/// serializer so the generated file reads in declaration order rather
/// than alphabetically. Unset keys have no place in that order (TOML has
/// no null), so they sort alphabetically after the set ones.
fn order() -> (Vec<String>, HashMap<String, Vec<String>>) {
    let skeleton = toml::to_string_pretty(&VelesConfig::default()).expect("serializes");
    let doc = skeleton
        .parse::<DocumentMut>()
        .expect("the serializer's own output parses");
    let mut sections = Vec::new();
    let mut keys: HashMap<String, Vec<String>> = HashMap::new();
    collect_order(doc.as_table(), String::new(), &mut sections, &mut keys);
    (sections, keys)
}

fn collect_order(
    table: &Table,
    prefix: String,
    sections: &mut Vec<String>,
    keys: &mut HashMap<String, Vec<String>>,
) {
    sections.push(prefix.clone());
    for (key, item) in table.iter() {
        let path = join(&prefix, key);
        match item {
            Item::Table(t) => collect_order(t, path, sections, keys),
            // Arrays of tables and arrays of scalars are leaves here:
            // their element shape is not part of the key order.
            _ => keys.entry(prefix.clone()).or_default().push(key.into()),
        }
    }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Where a knob belongs in declaration order: `(section, key)`. Unset
/// keys are not in the serializer's output at all, so they sort last
/// within their section — alphabetical among themselves, which is the
/// only stable answer available without a second source of truth.
fn position(
    sections: &[String],
    keys: &HashMap<String, Vec<String>>,
    knob: &Knob,
) -> (usize, usize) {
    let section = sections
        .iter()
        .position(|s| s == knob.section())
        .unwrap_or(sections.len());
    let key = keys
        .get(knob.section())
        .and_then(|ks| ks.iter().position(|k| k == knob.key()))
        .unwrap_or(usize::MAX);
    (section, key)
}

// ------------------------------------------------------------- defaults

/// The banner a printed surface carries: what the file is, and the one
/// convention a reader has to know — a commented key is an option whose
/// default is to be unset, not one that was forgotten.
const HEADER: &str = "\
# Every option this build of veles reads, at its default.
#
# Generated by `veles config defaults`. Merge into a config you already
# have with `veles config fill`, which documents what is missing and
# leaves everything you wrote — comments, values, ordering — alone.
#
# Nothing here is set. `# key = value  # default` shows an option at its
# built-in default: uncomment it to pin or change it, and leave it
# commented to follow the default as it changes. `# key =  # unset by
# default` is an option with no default — uncommenting it needs a value.
# One default depends on the machine: `[sandbox] backend` is \"native\"
# everywhere but Android, where it is \"auto\" — the userland package's
# boxes while it answers, \"native\" otherwise; name a backend to pin it.
# `# [[…]]` lines are arrays of tables — one block per entry, which is
# why they are never written as an empty `= []` (TOML would then refuse
# to let you expand them).";

/// A complete `config.toml` at defaults: every set option written, every
/// unset one named in a comment. Filling an empty document, in other
/// words — the same code path, so the two cannot disagree.
pub fn defaults_text() -> Result<String> {
    defaults_text_with(&[])
}

/// The same, with the options a connector reads out of its own blob.
pub fn defaults_text_with(extras: &[Knob]) -> Result<String> {
    Ok(format!("{HEADER}\n\n{}", fill_with("", extras)?.text))
}

/// The value `Fill::differing` reports as the default when an option has
/// none: leaving it out IS what this build does. Callers print it their own
/// way — `(default unset)` reads better than "(default unset by default)".
pub const UNSET_DEFAULT: &str = "unset by default";

/// What a merge did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fill {
    /// The document with every missing option documented.
    pub text: String,
    /// Options newly named in a comment (`# key = value  # default`, or
    /// `# key =` for one unset by default) — and ones whose default
    /// comment was refreshed because the default changed.
    pub documented: Vec<String>,
    /// Keys in the file that nothing reads.
    pub dead: Vec<String>,
    /// Keys whose value differs from the default: `(path, current, default)`.
    /// An option that has no default to compare against is reported against
    /// [`UNSET_DEFAULT`] rather than skipped — it is still something an
    /// operator chose.
    pub differing: Vec<(String, String, String)>,
}

impl Fill {
    /// Did the merge change anything at all?
    pub fn is_noop(&self) -> bool {
        self.documented.is_empty()
    }

    /// The line an auditor prints under the dead-key list, or `None` when
    /// there are no dead keys.
    ///
    /// This used to be a literal in the CLI ending with "the desktop reads
    /// some of them" — false for every key it was printed under: no Rust,
    /// Tcl or JS source reads any of them, which is the only reason they
    /// are on the list. A footnote that invents a consumer is how dead keys
    /// survive an audit, because it hands the next reader a reason not to
    /// delete them. So the note says the two things that are true — an
    /// unknown key is not an error, and removing them is the operator's
    /// call — and claims no reader.
    pub fn dead_key_note(&self) -> Option<String> {
        if self.dead.is_empty() {
            return None;
        }
        Some(
            "(left in place: an unknown key is not an error, and nothing in this build reads \
             these — deleting them is the operator's call, and `veles config fill` will not do \
             it for them, since a fill edits the document in place)"
                .to_string(),
        )
    }
}

/// Document every missing typed option in `text`.
pub fn fill(text: &str) -> Result<Fill> {
    fill_with(text, &[])
}

/// Merge every missing option into `text`, typed plus whatever the
/// connectors contribute for the blocks they read themselves.
///
/// Never re-serializes: the document is edited in place, so comments,
/// ordering and unknown-but-live blocks survive. A merge whose result
/// would not deserialize is refused — a config the agent cannot read is
/// worse than an incomplete one.
pub fn fill_with(text: &str, extras: &[Knob]) -> Result<Fill> {
    let doc = text
        .parse::<DocumentMut>()
        .with_context(|| "config.toml does not parse; fix it before filling")?;
    let mut surface = knobs();
    surface.extend(extras.iter().cloned());
    surface.sort_by(|a, b| a.path.cmp(&b.path));
    let by_path: HashMap<&str, &Knob> = surface.iter().map(|k| (k.path.as_str(), k)).collect();

    let mut report = Fill {
        text: String::new(),
        ..Default::default()
    };
    report.dead = dead_keys(&doc, &by_path);
    report.differing = differing(&doc, &surface);

    // Declaration order, so a freshly created section reads the way the
    // struct does rather than alphabetically. Sorting is stable, which is
    // what keeps the unset keys alphabetical inside their section.
    let (sections, key_order) = order();
    let mut ordered: Vec<&Knob> = surface.iter().collect();
    ordered.sort_by_key(|k| position(&sections, &key_order, k));

    // 1. what is missing: every option the file does not set.
    let mut comments: Vec<(String, String, String)> = Vec::new();
    for knob in ordered {
        let path: Vec<&str> = knob.path.split('.').collect();
        if crate::configedit::has(&doc, &path) {
            continue;
        }
        if let Some(clash) = ancestor_clash(&doc, &knob.path) {
            bail!(
                "`{clash}` is a value in this config, but `{}` needs it as a table",
                knob.path
            );
        }
        // A comment, never a value: a pinned default is a default the
        // code can no longer change. Uncommenting the line must still
        // give a value that parses as the option's type.
        if let Some(rendered) = knob.default.as_deref().filter(|_| knob.writable()) {
            rendered
                .parse::<Value>()
                .with_context(|| format!("default for `{}` is not a TOML value", knob.path))?;
        }
        comments.push((
            knob.section().to_string(),
            knob.comment(),
            knob.path.clone(),
        ));
    }

    // 2. they go in as text: a comment is not an item and the document
    //    cannot hold one.
    //    One already inside that section is not added again — `has` cannot
    //    see a comment, so without this a second fill grows the file
    //    forever. The check is scoped to the section because two sections
    //    legitimately hold the same line: `# provider =` belongs to both
    //    `[model]` and `[vision]`.
    let mut out = doc.to_string();
    let incoming = text;
    let mut unplaced: Vec<(String, String)> = Vec::new();
    for (section, line, path) in comments {
        // A connector block this config does not have is not created just
        // to hold a comment: `[platforms.irc]` turning up in a config that
        // never mentioned IRC reads like an adapter somebody enabled, and
        // an empty `platforms` map is not the same config as an absent one.
        let create = !under_free_form(&section);
        let key = path.rsplit('.').next().unwrap_or(&path);
        let refresh = format!("# {key} =");
        match insert_comment(&out, &section, &line, create, Some(&refresh)) {
            (text, Placed::Yes) => {
                out = text;
                report.documented.push(path);
            }
            (text, Placed::AlreadyThere) => out = text,
            (text, Placed::NoSection) => {
                out = text;
                // The tail block is regenerated every fill, so "documented"
                // has to mean NEWLY documented or a second fill reports the
                // same options forever.
                if !tail_has(incoming, &section, &line) {
                    report.documented.push(path.clone());
                }
                unplaced.push((section, line));
            }
        }
    }
    out = unplaced_block(&out, &unplaced);

    // 3. refuse a merge that would not load.
    toml::from_str::<VelesConfig>(&out)
        .context("the filled config does not deserialize; refusing to produce it")?;

    report.text = out;
    Ok(report)
}

/// Insert `line` at the end of `[section]`, or create the section.
/// Where a comment ended up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placed {
    /// Inserted into its section.
    Yes,
    /// The section already had that exact line, so nothing was written —
    /// which is what makes a second fill a no-op even though no parser can
    /// see a comment.
    AlreadyThere,
    /// The section does not exist and the caller asked not to create it.
    NoSection,
}

/// Insert `line` at the end of `[section]`, creating the section only when
/// `create` says so.
///
/// Text-level on purpose: the only way to add a comment to a TOML document
/// is to write the bytes. Insertion lands before the section's trailing
/// blank lines so the result still reads like a hand-written file, and
/// stops at the next header — including a deeper one, which is why `[acl]`
/// never swallows `[acl.roles]`.
///
/// `refresh` names a key (`# key =`): a `# key = …  # default` line already
/// in the section is REPLACED rather than kept beside the new one, so a
/// default that changed between builds updates its comment instead of
/// leaving two that disagree.
fn insert_comment(
    text: &str,
    section: &str,
    line: &str,
    create: bool,
    refresh: Option<&str>,
) -> (String, Placed) {
    let lines: Vec<&str> = text.lines().collect();
    let header = if section.is_empty() {
        None
    } else {
        Some(format!("[{section}]"))
    };

    let start = match &header {
        Some(h) => match lines.iter().position(|l| l.trim() == h.as_str()) {
            Some(i) => i + 1,
            None if create => {
                let mut out = text.trim_end().to_string();
                out.push_str(&format!("\n\n[{section}]\n{line}\n"));
                return (out, Placed::Yes);
            }
            None => return (text.to_string(), Placed::NoSection),
        },
        // Top level: the block before the first header.
        None => 0,
    };

    let end = match &header {
        Some(_) => lines[start..]
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .map(|i| start + i)
            .unwrap_or(lines.len()),
        None => lines
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .unwrap_or(lines.len()),
    };

    if lines[start..end].iter().any(|l| l.trim() == line.trim()) {
        return (text.to_string(), Placed::AlreadyThere);
    }
    if let Some(prefix) = refresh {
        let stale = (start..end).find(|&i| {
            let t = lines[i].trim();
            t.starts_with(prefix) && t.ends_with("# default")
        });
        if let Some(i) = stale {
            let indent = &lines[i][..lines[i].len() - lines[i].trim_start().len()];
            let fresh = format!("{indent}{}", line.trim());
            let mut out: Vec<&str> = lines.clone();
            out[i] = &fresh;
            let mut joined = out.join("\n");
            if text.ends_with('\n') {
                joined.push('\n');
            }
            return (joined, Placed::Yes);
        }
    }

    // Walk back over blank lines so the comment joins the section body.
    let mut at = end;
    while at > start && lines[at - 1].trim().is_empty() {
        at -= 1;
    }

    let mut out: Vec<&str> = Vec::with_capacity(lines.len() + 1);
    out.extend_from_slice(&lines[..at]);
    out.push(line);
    out.extend_from_slice(&lines[at..]);
    let mut joined = out.join("\n");
    if text.ends_with('\n') || !joined.ends_with('\n') {
        joined.push('\n');
    }
    (joined, Placed::Yes)
}

/// The marker the generated tail block starts with.
const UNPLACED: &str = "# Options in blocks this config does not have.";

/// Is this option already listed in the generated tail block? Scoped to its
/// section header, for the same reason the in-section check is: two blocks
/// can legitimately hold the same line.
fn tail_has(text: &str, section: &str, line: &str) -> bool {
    let Some(start) = text.lines().position(|l| l.trim() == UNPLACED) else {
        return false;
    };
    let want = format!("#{line}");
    let header = format!("# [{section}]");
    let mut in_section = false;
    for l in text.lines().skip(start) {
        let t = l.trim();
        if t == header {
            in_section = true;
            continue;
        }
        if t.starts_with("# [") {
            in_section = false;
            continue;
        }
        if in_section && t == want.trim() {
            return true;
        }
    }
    false
}

/// Options whose block this file does not have, listed at the end instead
/// of conjured into existence.
///
/// Regenerated rather than appended: the block is derived, so replacing it
/// is what keeps a second fill byte-identical and a changed default from
/// going stale.
fn unplaced_block(text: &str, unplaced: &[(String, String)]) -> String {
    let marker_at = text.lines().position(|l| l.trim() == UNPLACED);
    if unplaced.is_empty() && marker_at.is_none() {
        return text.to_string();
    }
    let head = match marker_at {
        Some(i) => text.lines().take(i).collect::<Vec<_>>().join("\n"),
        None => text.to_string(),
    };
    let mut out = head.trim_end().to_string();
    if !unplaced.is_empty() {
        out.push_str(&format!(
            "\n\n{UNPLACED}\n# Nothing below is active: create the block and \
             uncomment the key you mean.\n"
        ));
        let mut last = String::new();
        for (section, line) in unplaced {
            if *section != last {
                let head = if section.is_empty() {
                    String::new()
                } else {
                    format!("[{section}]")
                };
                out.push_str(&format!("#\n# {head}\n"));
                last = section.clone();
            }
            out.push_str(&format!("#{line}\n"));
        }
    }
    out.push('\n');
    out
}

// ----------------------------------------------------------------- audit

/// The nearest ancestor of `path` this config holds as a value rather than
/// a table.
///
/// `model = "openrouter"` is a plausible thing for a person to write, and
/// filling `model.provider` underneath it yields a duplicate key that only
/// surfaces the next time anything reads the file. Naming the clash is the
/// useful answer; producing the file is not.
fn ancestor_clash(doc: &DocumentMut, path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('.').collect();
    (1..parts.len()).find_map(|i| {
        let value = crate::configedit::get_json(doc, &parts[..i])?;
        (!value.is_object()).then(|| parts[..i].join("."))
    })
}

/// Keys in the document that nothing reads.
fn dead_keys(doc: &DocumentMut, surface: &HashMap<&str, &Knob>) -> Vec<String> {
    let (sections, _) = order();
    let known_tables: BTreeSet<String> = sections.into_iter().collect();
    let open = open_maps();
    let paths: HashSet<&str> = surface.keys().copied().collect();
    let mut out = Vec::new();
    walk_dead(
        doc.as_table(),
        String::new(),
        surface,
        &known_tables,
        &open,
        &paths,
        false,
        &mut out,
    );
    out.sort();
    out
}

/// `in_blob` says whether this table sits inside a block some connector
/// listed. Outside one, a free-form root gets no verdicts at all; inside
/// one, everything unlisted is inert — which is how a hermes
/// `[platforms.telegram.home_channel]` block gets named instead of
/// surviving an audit silently.
#[allow(clippy::too_many_arguments)]
fn walk_dead(
    table: &Table,
    prefix: String,
    surface: &HashMap<&str, &Knob>,
    known_tables: &BTreeSet<String>,
    open: &[String],
    paths: &HashSet<&str>,
    in_blob: bool,
    out: &mut Vec<String>,
) {
    if under_open_map(&prefix, open) {
        return;
    }
    let judgable = !under_free_form(&prefix) || in_blob;
    if !prefix.is_empty() && !judgable {
        return;
    }
    if !prefix.is_empty()
        && table.is_empty()
        && !known_tables.contains(&prefix)
        && !covered(&prefix, paths)
    {
        out.push(prefix.clone());
        return;
    }
    for (key, item) in table.iter() {
        let path = join(&prefix, key);
        match item {
            Item::Table(t) => {
                let child_covered = covered(&path, paths);
                if prefix == "platforms" && !child_covered {
                    // No adapter in this build reads that platform NAME, so
                    // every key inside the block is inert. Hermes called IRC
                    // `ircx`; a config carried over from it says
                    // `enabled = true` under a name nobody looks up.
                    out.push(path);
                } else {
                    let child_blob = child_covered || (in_blob && under_free_form(&path));
                    walk_dead(t, path, surface, known_tables, open, paths, child_blob, out);
                }
            }
            // Auditing an array of tables needs its element schema; see
            // the module header.
            Item::ArrayOfTables(_) => {}
            Item::Value(_) => {
                if judgable && !surface.contains_key(path.as_str()) && !under_open_map(&path, open)
                {
                    out.push(path);
                }
            }
            Item::None => {}
        }
    }
}

/// Options the file sets to something other than the default.
///
/// Arrays of tables are reported as a count: a provider list rendered
/// inline is a page of noise where "8 entries" is the answer.
fn differing(doc: &DocumentMut, surface: &[Knob]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for knob in surface {
        let path: Vec<&str> = knob.path.split('.').collect();
        let Some(got_json) = crate::configedit::get_json(doc, &path) else {
            continue;
        };
        if knob.shape == Shape::TableArray {
            let entries = got_json.as_array().map(Vec::len).unwrap_or(0);
            if entries > 0 {
                out.push((
                    knob.path.clone(),
                    format!(
                        "{entries} {}",
                        if entries == 1 { "entry" } else { "entries" }
                    ),
                    "none declared".into(),
                ));
            }
            continue;
        }
        let Some(got) = literal(&got_json) else {
            continue;
        };
        // An option whose default is UNSET is still a difference when the
        // file sets it — `acl.default_role = "user"` is the whole posture
        // of an installation, and skipping it because there is no default
        // to compare against would hide the one line an operator most
        // needs to see named.
        let Some(want) = &knob.default else {
            out.push((knob.path.clone(), got, UNSET_DEFAULT.into()));
            continue;
        };
        if same_value(&got, want) {
            continue;
        }
        out.push((knob.path.clone(), got, want.clone()));
    }
    out
}

/// Compare two rendered TOML values without demanding byte equality:
/// `85` and `85.0` are the same number, and `"a"` written with single
/// quotes is the same string.
fn same_value(got: &str, want: &str) -> bool {
    if got == want {
        return true;
    }
    match (got.parse::<Value>(), want.parse::<Value>()) {
        (Ok(a), Ok(b)) => match (a.as_float(), b.as_float()) {
            (Some(x), Some(y)) => x == y,
            _ => a.to_string() == b.to_string(),
        },
        _ => false,
    }
}

/// Count of the surface, for a status line: `(options, unset ones, ones
/// shown with a default but deliberately not written)`.
pub fn counts(extras: &[Knob]) -> (usize, usize, usize) {
    let mut all = knobs();
    all.extend(extras.iter().cloned());
    let unset = all.iter().filter(|k| k.default.is_none()).count();
    let shown = all
        .iter()
        .filter(|k| !k.writable() && k.default.is_some())
        .count();
    (all.len(), unset, shown)
}

// ------------------------------------------------------------- env vars

/// An environment variable `.env` can set, and what the code falls back to
/// when it stays unset.
///
/// Reachability is not what earns a var a place here: `config::load()`
/// hands this file to `load_env_file`, which `set_var`s every uncommented
/// `KEY=value`, so nearly any var works from `.env`. What earns the place
/// is that the fallback named below is pinned by a test against the reader
/// that uses it — a documented default nobody checked is the same lie as a
/// dead config key.
#[derive(Debug, Clone, PartialEq)]
pub struct EnvKnob {
    pub name: String,
    /// The value used when the var is unset. `None` means there is nothing
    /// to show: a secret, or a feature that stays off.
    pub default: Option<String>,
    pub note: String,
}

impl EnvKnob {
    /// A var the code falls back from, with the fallback named.
    pub fn with_default(name: &str, default: &str, note: &str) -> EnvKnob {
        EnvKnob {
            name: name.to_string(),
            default: Some(default.to_string()),
            note: note.to_string(),
        }
    }

    /// A var with nothing to fall back to: unset means off, or absent.
    pub fn without_default(name: &str, note: &str) -> EnvKnob {
        EnvKnob {
            name: name.to_string(),
            default: None,
            note: note.to_string(),
        }
    }

    /// The lines a fill appends: the note, then the assignment.
    ///
    /// Two lines, not one, because dotenv has no trailing comment —
    /// `A2A_PORT=9900  # the port` uncommented is the value
    /// `9900  # the port`, which parses as nothing and falls back silently.
    pub fn comment(&self) -> String {
        let value = self.default.as_deref().unwrap_or("");
        format!("# {}\n# {}={}", self.note, self.name, value)
    }
}

/// The vars core reads itself, and the fallback each one has.
pub fn env_knobs() -> Vec<EnvKnob> {
    vec![
        EnvKnob::without_default(
            "A2A_BEARER_TOKEN",
            "unset: inbound identities are ip:<addr>, and the bind stays loopback",
        ),
        EnvKnob::without_default(
            "A2A_HOST",
            "process env beats .env beats platforms.a2a.extra.host; unset binds 127.0.0.1, widening needs a token too",
        ),
        EnvKnob::with_default("A2A_MAX_PINGPONG_TURNS", "5", "clamped to 1..=20"),
        EnvKnob::without_default("A2A_PEER_TOKENS", "named peers: name:token,name:token"),
        EnvKnob::with_default(
            "A2A_PORT",
            "9900",
            "process env beats .env beats platforms.a2a.extra.port",
        ),
        EnvKnob::without_default(
            "A2A_RATE_LIMIT",
            "requests per identity per minute; unset is unlimited",
        ),
        EnvKnob::without_default(
            "A2A_TRUSTED_PEERS",
            "comma-separated identities; empty trusts any authenticated peer",
        ),
        EnvKnob::without_default(
            "ALIBABA_CONSOLE_TOKEN",
            "the personal plan's console session; unset reads [tokenplan] console_config",
        ),
        EnvKnob::without_default(
            "DESKTOP_BEARER_TOKEN",
            "the gateway mints one at serve time when this is unset",
        ),
        EnvKnob::without_default(
            "DESKTOP_PEER_TOKENS",
            "named peers for another machine's desktop: name:token",
        ),
        EnvKnob::without_default(
            "VELES_API_KEY",
            "the provider key when no provider entry sets key_env",
        ),
        EnvKnob::without_default(
            "VELES_DESKTOP_HOST",
            "process env beats .env beats platforms.desktop.extra.host; unset binds 127.0.0.1, widening needs a token too",
        ),
        EnvKnob::with_default(
            "VELES_DESKTOP_PORT",
            "8642",
            "process env beats .env beats platforms.desktop.extra.port",
        ),
        EnvKnob::without_default(
            "VELES_CHALLENGE_URL",
            "unset: a relay page links to this listener's own loopback URL",
        ),
        EnvKnob::without_default(
            "VELES_PROXY_TOKEN",
            "unset: the local proxy takes any caller; set, it wants this bearer",
        ),
        EnvKnob::without_default(
            "VELES_SIGNER_TOKEN",
            "what this gateway presents to [cluster] signers; unset: they answer 401",
        ),
        EnvKnob::with_default(
            "VELES_USER_AGENT",
            &format!("veles-agent/{}", env!("CARGO_PKG_VERSION")),
            "presets hermes and claw; anything else is a literal UA",
        ),
    ]
}

/// Vars that decide where `.env` itself is read from.
///
/// Setting one here is not inert, which is worse than inert: the line is
/// exported, and every `config_dir()` after it points somewhere else — so
/// the config that was just loaded came from one directory while the state
/// that follows comes from another.
pub fn chosen_before_the_file() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "VELES_HOME",
            "forces config, data and cache under one directory",
        ),
        (
            "XDG_CONFIG_HOME",
            "where config.toml — and this file — are looked for",
        ),
        (
            "XDG_DATA_HOME",
            "where state.db (sessions and memory) and the rest of the state lives",
        ),
    ]
}

/// What an `.env` merge did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvFill {
    /// The file with every var it did not mention documented.
    pub text: String,
    /// Vars newly documented, as commented lines.
    pub added: Vec<String>,
    /// Vars already carrying a value. Never touched: a live credential or
    /// a live port is the one thing a fill must not rewrite.
    pub live: Vec<String>,
    /// Live assignments that chose the file they are sitting in.
    pub misplaced: Vec<(String, String)>,
}

impl EnvFill {
    /// Did the merge change anything?
    pub fn is_noop(&self) -> bool {
        self.added.is_empty()
    }
}

/// The banner an appended block carries.
const ENV_HEADER: &str = "\
# Vars this build of veles reads, at the value it falls back to.
#
# Uncommenting a line assigns it. The note is on its own line above
# because dotenv has no trailing comment: `NAME=9900  # the port` is the
# value `9900  # the port`. A `# NAME=` with nothing after it is a var
# whose default is unset — uncommenting it needs a value.
#
# None of this is required, and none of it is the whole list:
# `config::load()` exports this file into the process environment, so
# other vars work from here too. These are the ones whose fallback is
# pinned against the reader that uses it.";

/// The name a line assigns to, if it assigns one.
///
/// A commented line still counts: it names the var, so a second fill has
/// nothing to add. Notes are skipped by shape — `# clamped to 1..=20`
/// contains an `=` and is not an assignment.
fn assigned(line: &str) -> Option<(String, bool)> {
    let t = line.trim();
    let (head, _) = t.split_once('=')?;
    let commented = head.trim_start().starts_with('#');
    let name = head.trim().trim_start_matches('#').trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some((name.to_string(), commented))
}

/// Document every var `.env` does not mention yet, as commented lines.
///
/// Appends only: an existing line is never rewritten, reordered or
/// uncommented, because the file holds live credentials. `extras` are the
/// vars a connector reads.
pub fn env_fill(text: &str, extras: &[EnvKnob]) -> EnvFill {
    let mut all = env_knobs();
    all.extend(extras.iter().cloned());

    let mut out = EnvFill {
        text: text.to_string(),
        ..Default::default()
    };

    let mut live: HashSet<String> = HashSet::new();
    let mut mentioned: HashSet<String> = HashSet::new();
    for line in text.lines() {
        if let Some((name, commented)) = assigned(line) {
            mentioned.insert(name.clone());
            if !commented {
                live.insert(name);
            }
        }
    }

    let chosen: HashMap<&str, &str> = chosen_before_the_file().into_iter().collect();
    for name in &live {
        if let Some(what) = chosen.get(name.as_str()) {
            out.misplaced.push((name.clone(), what.to_string()));
        }
    }
    out.misplaced.sort();

    let mut block = String::new();
    for knob in &all {
        if live.contains(&knob.name) {
            out.live.push(knob.name.clone());
            continue;
        }
        if mentioned.contains(&knob.name) {
            continue;
        }
        if !block.is_empty() {
            block.push('\n');
        }
        block.push_str(&knob.comment());
        block.push('\n');
        out.added.push(knob.name.to_string());
    }
    out.live.sort();

    if block.is_empty() {
        return out;
    }
    if !out.text.is_empty() {
        if !out.text.ends_with('\n') {
            out.text.push('\n');
        }
        out.text.push('\n');
    }
    out.text.push_str(ENV_HEADER);
    out.text.push('\n');
    out.text.push_str(&block);
    out
}

/// Every var, documented — what a fresh `.env` would look like.
pub fn env_defaults_text(extras: &[EnvKnob]) -> String {
    let body = env_fill("", extras);
    body.text
}

/// What [`tidy`] did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tidy {
    /// The document with every restated default turned into a comment.
    pub text: String,
    /// `(path, line before, line after)` for each line commented out.
    pub commented: Vec<(String, String, String)>,
    /// `(path, why)` for lines equal to their default that stay live.
    pub kept: Vec<(String, String)>,
}

impl Tidy {
    pub fn is_noop(&self) -> bool {
        self.commented.is_empty()
    }
}

/// Turn every live line that only restates its built-in default into the
/// comment that documents it: `key = 4` becomes `# key = 4  # default`.
///
/// What an older `veles config fill` wrote — every default as a value —
/// pinned the install to the defaults of the day it ran: a default the
/// code later changed never reached it. Run again, this refreshes nothing
/// (a commented line is not a value); `fill` refreshes stale comments.
///
/// The parser finds the lines (`toml_edit`'s spans); only lines it places
/// alone, as `key = value` in their own section's table, are touched. A
/// dotted key, an inline table, a multi-line value — those stay live and
/// are named in `kept`. And each change is proven neutral before it is
/// made: the config with that one line commented must deserialize to
/// exactly what it did before, or the line stays.
pub fn tidy(text: &str) -> Result<Tidy> {
    let before: VelesConfig = toml::from_str(text)
        .context("config.toml does not load as a config; fix it before tidying")?;
    let want = serde_json::to_value(&before)?;
    let im = toml_edit::ImDocument::parse(text)
        .map_err(|e| anyhow::anyhow!("config.toml does not parse: {e}"))?;
    let mut report = Tidy::default();
    // (line range, replacement, path, original line)
    let mut edits: Vec<(std::ops::Range<usize>, String, String, String)> = Vec::new();
    for knob in knobs() {
        if !knob.writable() {
            continue;
        }
        let Some(default) = knob.default.as_deref() else {
            continue;
        };
        let segs: Vec<&str> = knob.path.split('.').collect();
        let (parents, leaf) = segs.split_at(segs.len() - 1);
        let mut table = im.as_table();
        let mut reached = true;
        for seg in parents {
            match table.get(seg) {
                Some(Item::Table(t)) => table = t,
                _ => {
                    reached = false;
                    break;
                }
            }
        }
        if !reached {
            continue;
        }
        let Some((key, Item::Value(value))) = table.get_key_value(leaf[0]) else {
            continue;
        };
        let got = value.clone().decorated("", "").to_string();
        if !same_value(&got, default) {
            continue;
        }
        let (Some(ks), Some(vs)) = (key.span(), value.span()) else {
            report
                .kept
                .push((knob.path.clone(), "the parser gave no position".into()));
            continue;
        };
        let line_start = text[..ks.start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let line_end = text[vs.end..]
            .find('\n')
            .map(|i| vs.end + i)
            .unwrap_or(text.len());
        let indent = &text[line_start..ks.start];
        let tail = text[vs.end..line_end].trim();
        if !indent.trim().is_empty() || text[ks.start..vs.end].contains('\n') {
            report.kept.push((
                knob.path.clone(),
                "not alone on its line (a dotted key, an inline table or a multi-line value)"
                    .into(),
            ));
            continue;
        }
        if !(tail.is_empty() || tail.starts_with('#')) {
            continue;
        }
        let mut line = format!("{indent}{}", knob.comment().trim_end());
        if !tail.is_empty() {
            // The operator's own note on the line rides along.
            line.push(' ');
            line.push_str(tail);
        }
        let candidate = format!("{}{line}{}", &text[..line_start], &text[line_end..]);
        let neutral = toml::from_str::<VelesConfig>(&candidate)
            .ok()
            .and_then(|c| serde_json::to_value(&c).ok())
            .is_some_and(|v| v == want);
        if !neutral {
            report.kept.push((
                knob.path.clone(),
                "removing it changes the loaded config (the key's absence does not mean its default)"
                    .into(),
            ));
            continue;
        }
        let original = text[line_start..line_end].to_string();
        edits.push((line_start..line_end, line, knob.path.clone(), original));
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0.start));
    let mut out = text.to_string();
    for (range, line, path, original) in &edits {
        out.replace_range(range.clone(), line);
        report.commented.push((
            path.clone(),
            original.trim().to_string(),
            line.trim().to_string(),
        ));
    }
    report.commented.reverse();
    let after: VelesConfig =
        toml::from_str(&out).context("the tidied config does not load; refusing to produce it")?;
    if serde_json::to_value(&after)? != want {
        bail!("the tidied config would load differently; refusing to produce it");
    }
    report.text = out;
    Ok(report)
}

/// `veles config tidy`: tidy the file at `path`; with `apply`, write it
/// with configedit's one write rule (a `.bak` beside it with the file's
/// mode, the file keeping its own).
/// Returns the report and, when it wrote, the backup's path.
pub fn tidy_file(
    path: &std::path::Path,
    apply: bool,
) -> Result<(Tidy, Option<std::path::PathBuf>)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let report = tidy(&text)?;
    let backup = if apply && !report.is_noop() {
        crate::configedit::write_with_backup(path, &text, &report.text)?
    } else {
        None
    };
    Ok((report, backup))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> String {
        fill("").expect("fills").text
    }

    /// A stand-in for what a connector crate contributes: the same shapes,
    /// without core depending on one.
    fn irc_extras() -> Vec<Knob> {
        vec![
            Knob::blob_documented(
                "platforms.irc.server",
                "\"127.0.0.1\"",
                "IRC_SERVER wins over this",
            ),
            Knob::blob_documented("platforms.irc.port", "6667", "IRC_PORT wins over this"),
            Knob::blob("platforms.irc.name", None, Some("defaults to `server`")),
            Knob::blob_array("platforms.irc.networks"),
        ]
    }

    #[test]
    fn a_platform_no_adapter_reads_is_named_once_not_key_by_key() {
        let report = fill_with("[platforms.ircx]\nenabled = true\n", &irc_extras()).expect("fills");
        assert!(
            report.dead.contains(&"platforms.ircx".to_string()),
            "hermes called IRC `ircx`; the block is inert: {:?}",
            report.dead
        );
        assert!(
            !report.dead.iter().any(|d| d == "platforms.ircx.enabled"),
            "the platform name is the mistake, not each key under it: {:?}",
            report.dead
        );
    }

    #[test]
    fn an_unlisted_key_inside_a_listed_block_is_dead() {
        let text = "\
[platforms.irc]
server = \"irc.libera.chat\"
require_mention = true

[platforms.irc.home_channel]
chat_id = \"1\"

[platforms.irc.hermes_leftover]
key = 1
";
        // The fill surface as `veles config fill` composes it: the core
        // platform table plus each connector listing.
        let mut extras = platform_surface();
        extras.extend(irc_extras());
        let report = fill_with(text, &extras).expect("fills");
        assert!(
            !report.dead.contains(&"platforms.irc.server".to_string()),
            "a listed key is read: {:?}",
            report.dead
        );
        assert!(
            report
                .dead
                .contains(&"platforms.irc.require_mention".to_string()),
            "this surface lists no such key, so nothing reads it: {:?}",
            report.dead
        );
        assert!(
            report
                .dead
                .contains(&"platforms.irc.hermes_leftover.key".to_string()),
            "coverage reaches the whole listed block, not just its leaves — \
             a hermes sub-block survives an audit otherwise: {:?}",
            report.dead
        );
        assert!(
            !report
                .dead
                .contains(&"platforms.irc.home_channel.chat_id".to_string()),
            "the home table reads every platform's home_channel.chat_id, so \
             platform_surface lists IRC's (2026-10-21): {:?}",
            report.dead
        );
        assert!(
            report
                .differing
                .iter()
                .any(|(p, got, _)| p == "platforms.irc.server" && got == "\"irc.libera.chat\""),
            "a blob key with a default is compared like any other: {:?}",
            report.differing
        );
    }

    #[test]
    fn a_block_the_config_does_not_have_is_not_conjured_into_existence() {
        let report = fill_with("", &irc_extras()).expect("fills");
        assert!(
            !report.text.contains("\n[platforms.irc]"),
            "an empty platforms map is not the config an absent one is:\n{}",
            report.text
        );
        assert!(
            report.text.contains(UNPLACED),
            "but the options are still listed:\n{}",
            report.text
        );
        assert!(
            report.text.contains("# [platforms.irc]"),
            "under the block they belong to:\n{}",
            report.text
        );
        assert!(
            report.text.contains("## server = \"127.0.0.1\"")
                || report.text.contains("# # server ="),
            "with their defaults, so uncommenting works:\n{}",
            report.text
        );
        let parsed: VelesConfig = toml::from_str(&report.text).expect("parses");
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            serde_json::to_value(VelesConfig::default()).unwrap(),
            "the tail block is all comments: it must not change the config"
        );
    }

    #[test]
    fn the_tail_block_is_regenerated_rather_than_duplicated() {
        let once = fill_with("", &irc_extras()).expect("fills").text;
        let twice = fill_with(&once, &irc_extras()).expect("fills");
        assert_eq!(twice.text, once, "byte identical on a second fill");
        assert!(twice.is_noop(), "{:?}", twice.documented);
        assert_eq!(
            once.matches(UNPLACED).count(),
            1,
            "one tail block, not one per fill"
        );
    }

    #[test]
    fn without_a_connector_surface_the_blobs_get_no_verdicts() {
        // `fill` on its own knows nothing about IRC, so it must not call
        // the block dead: a false verdict is worse than no verdict.
        let report = fill("[platforms.irc]\nserver = \"irc.libera.chat\"\n").expect("fills");
        assert!(report.dead.is_empty(), "{:?}", report.dead);
    }

    #[test]
    fn cores_own_platform_blocks_are_listed_and_documented() {
        let surface = platform_surface();
        let paths: Vec<&str> = surface.iter().map(|k| k.path.as_str()).collect();
        assert!(paths.contains(&"platforms.a2a.enabled"), "{paths:?}");
        assert!(paths.contains(&"platforms.desktop.extra.port"), "{paths:?}");
        assert!(
            platform_surface().iter().all(|k| !k.writable()),
            "a fill shows them; an env var or an absent block decides them"
        );
    }

    #[test]
    fn surface_holds_the_options_a_deployed_instance_silently_inherits() {
        let all = knobs();
        let paths: HashSet<&str> = all.iter().map(|k| k.path.as_str()).collect();
        for path in [
            "acl.lockdown",
            "acl.owner",
            "sandbox.backend",
            "sandbox.admin_default_zone",
            "agent.compaction.keep_recent",
            "tools.tier",
        ] {
            assert!(paths.contains(path), "{path} must be on the surface");
        }
        assert!(
            paths.len() > 60,
            "the surface is not a stub: {}",
            paths.len()
        );
    }

    #[test]
    fn unset_options_have_no_default_to_write() {
        let all = knobs();
        let by_path: HashMap<&str, &Knob> = all.iter().map(|k| (k.path.as_str(), k)).collect();
        for path in ["model.provider", "tui.theme", "agent.reasoning_effort"] {
            assert_eq!(
                by_path[path].default, None,
                "{path} is an Option: unset is its default"
            );
        }
        assert_eq!(by_path["acl.lockdown"].default.as_deref(), Some("false"));
        // Machine-dependent: the phone's default is `auto` (the userland
        // where it answers), and the printed header says so on every
        // machine, since the file is read on others.
        assert_eq!(
            by_path["sandbox.backend"].default.as_deref(),
            Some(if cfg!(target_os = "android") {
                "\"auto\""
            } else {
                "\"native\""
            })
        );
        let text = defaults_text().expect("renders");
        assert!(
            text.contains("`[sandbox] backend` is \"native\"")
                && text.contains("Android, where it is \"auto\""),
            "the header names the phone's default:\n{text}"
        );
    }

    #[test]
    fn a_percentage_stays_a_float() {
        let all = knobs();
        let by_path: HashMap<&str, &Knob> = all.iter().map(|k| (k.path.as_str(), k)).collect();
        let trigger = by_path["agent.compaction.trigger_percent"]
            .default
            .as_deref()
            .expect("set");
        assert!(
            trigger.contains('.'),
            "{trigger} must render as a float or TOML reads an integer"
        );
    }

    #[test]
    fn defaults_deserialize_to_the_default_config() {
        let text = filled();
        let parsed: VelesConfig = toml::from_str(&text).expect("parses");
        assert_eq!(
            serde_json::to_value(&parsed).unwrap(),
            serde_json::to_value(VelesConfig::default()).unwrap(),
            "the generated file must mean exactly what the defaults mean"
        );
    }

    #[test]
    fn defaults_name_every_option_including_the_unset_ones() {
        let text = filled();
        for knob in knobs() {
            assert!(
                text.contains(knob.key()),
                "`{}` is missing from the generated config:\n{text}",
                knob.path
            );
        }
        assert!(
            text.contains("# provider ="),
            "an unset option is named as a comment, not written"
        );
    }

    #[test]
    fn defaults_hold_no_array_of_tables_as_an_empty_array() {
        let text = filled();
        assert!(
            !text.contains("entries = []"),
            "`acl.entries = []` would block the user from ever writing [[acl.entries]]:\n{text}"
        );
        // The trap is gone: expanding one still parses.
        let expanded = format!("{text}\n[[acl.entries]]\nidentity = \"a!*@*\"\nrole = \"admin\"\n");
        assert!(
            toml::from_str::<VelesConfig>(&expanded).is_ok(),
            "a filled config must still accept [[acl.entries]]"
        );
    }

    /// USER 2026-10-04: "yes but document the defaults in the file as
    /// comments". A fill pins nothing: a missing option arrives as the
    /// comment that shows its default, and the file means the defaults.
    #[test]
    fn a_fill_documents_a_default_and_pins_nothing() {
        let report = fill("[acl]\n").expect("fills");
        assert!(
            report.text.contains("# lockdown = false") && report.text.contains("# default"),
            "{}",
            report.text
        );
        assert!(
            !report
                .text
                .lines()
                .any(|l| l.trim_start().starts_with("lockdown")),
            "no live line: {}",
            report.text
        );
    }

    /// A default that changed between builds updates its comment rather
    /// than leaving the old one beside the new.
    #[test]
    fn a_changed_default_refreshes_its_comment() {
        let stale = "[acl]\n# lockdown = true       # default\n";
        let report = fill(stale).expect("fills");
        assert!(
            !report.text.contains("# lockdown = true"),
            "the stale default is gone:\n{}",
            report.text
        );
        assert_eq!(
            report.text.matches("# lockdown =").count(),
            1,
            "one comment per key:\n{}",
            report.text
        );
        assert!(report.documented.contains(&"acl.lockdown".to_string()));
    }

    /// The live config of an install that ran an older fill: defaults
    /// written as values. Tidy turns exactly those into comments.
    #[test]
    fn tidy_comments_out_a_restated_default_and_nothing_else() {
        let text = "\
[acl]
lockdown = false   # mine
default_role = \"user\"

[sandbox]
backend = \"native\"

[agent.compaction]
keep_recent = 99
";
        let t = tidy(text).expect("tidies");
        let paths: Vec<&str> = t.commented.iter().map(|c| c.0.as_str()).collect();
        assert!(paths.contains(&"acl.lockdown"), "{paths:?}");
        assert!(paths.contains(&"sandbox.backend"), "{paths:?}");
        assert!(
            !paths.contains(&"agent.compaction.keep_recent"),
            "a changed value stays live: {paths:?}"
        );
        assert!(
            t.text.contains("# lockdown = false") && t.text.contains("# mine"),
            "the operator's own note rides along:\n{}",
            t.text
        );
        assert!(
            t.text.lines().any(|l| l.starts_with("default_role = ")),
            "an option unset by default is the operator's choice and stays:\n{}",
            t.text
        );
        assert!(t.text.contains("keep_recent = 99"), "{}", t.text);
        let a: VelesConfig = toml::from_str(text).unwrap();
        let b: VelesConfig = toml::from_str(&t.text).unwrap();
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            serde_json::to_value(&b).unwrap(),
            "a tidy never changes what the config means"
        );
        let again = tidy(&t.text).expect("tidies");
        assert!(again.is_noop(), "{:?}", again.commented);
    }

    /// Only a line the parser places alone is touched.
    #[test]
    fn tidy_leaves_a_dotted_key_live_and_says_so() {
        let text = "[acl]\nlockdown = false\n\n[agent]\ncompaction.keep_recent = 4\n";
        let default_keep = knobs()
            .into_iter()
            .find(|k| k.path == "agent.compaction.keep_recent")
            .and_then(|k| k.default)
            .expect("keep_recent has a default");
        let text = text.replace("= 4", &format!("= {default_keep}"));
        let t = tidy(&text).expect("tidies");
        assert!(
            t.text
                .contains(&format!("compaction.keep_recent = {default_keep}")),
            "{}",
            t.text
        );
        assert!(
            t.kept
                .iter()
                .any(|(p, _)| p == "agent.compaction.keep_recent"),
            "{:?}",
            t.kept
        );
    }

    #[test]
    fn filling_a_filled_file_changes_nothing() {
        let text = filled();
        let again = fill(&text).expect("fills");
        assert!(again.is_noop(), "documented={:?}", again.documented);
        assert_eq!(again.text, text, "idempotent byte for byte");
    }

    #[test]
    fn filling_keeps_comments_values_and_unknown_blocks() {
        let text = "\
# my box
[model]
provider = \"openrouter\"   # chosen on purpose

[ui]
font_scale = 1.2
";
        let report = fill(text).expect("fills");
        assert!(
            report.text.contains("# my box"),
            "the header comment survives"
        );
        assert!(
            report
                .text
                .contains("provider = \"openrouter\"   # chosen on purpose"),
            "the value and its trailing comment survive:\n{}",
            report.text
        );
        assert!(
            report.text.contains("font_scale = 1.2"),
            "the desktop block survives"
        );
        assert!(
            report.documented.contains(&"acl.lockdown".to_string())
                && report.text.contains("# lockdown = false"),
            "missing keys are documented at their default:\n{}",
            report.text
        );
        assert!(
            !report.documented.iter().any(|p| p.starts_with("ui.")),
            "the free-form desktop block is not filled"
        );
        let parsed: VelesConfig = toml::from_str(&report.text).expect("parses");
        assert_eq!(parsed.model.provider.as_deref(), Some("openrouter"));
    }

    #[test]
    fn a_missing_section_is_created_where_its_keys_go() {
        let report = fill("[model]\nprovider = \"x\"\n").expect("fills");
        let sandbox_at = report
            .text
            .find("[sandbox]")
            .expect("the fence section is created");
        // LINE-ANCHORED: `find("backend =")` matched inside
        // `[cluster] delegate_backend =` (B4) once that key existed,
        // and the test failed for a reason it was not written to
        // catch. A bare substring search cannot tell "the sandbox
        // backend key" from any key that ends with the word.
        let backend_at = report.text.find("\n# backend =").expect("with its keys") + 1;
        assert!(sandbox_at < backend_at, "the header precedes its keys");
        let after: &str = &report.text[backend_at..];
        assert!(
            after.lines().next().unwrap_or("").contains("backend ="),
            "the key is inside its own section, not appended to another:\n{}",
            report.text
        );
    }

    #[test]
    fn keys_nothing_reads_are_named_dead() {
        let text = "\
[security]
redact_secrets = false
allow_private_urls = true

[agent.compaction]
keep_recent = 3
";
        let report = fill(text).expect("fills");
        assert!(
            report.dead.contains(&"security.redact_secrets".to_string()),
            "hermes legacy: {:?}",
            report.dead
        );
        assert!(report
            .dead
            .contains(&"security.allow_private_urls".to_string()));
        assert!(
            !report.dead.iter().any(|d| d.starts_with("agent.")),
            "a real key is not dead: {:?}",
            report.dead
        );
    }

    #[test]
    fn a_bare_unknown_table_is_dead_too() {
        let report = fill("[nosuch]\n").expect("fills");
        assert!(
            report.dead.contains(&"nosuch".to_string()),
            "{:?}",
            report.dead
        );
    }

    #[test]
    fn values_that_differ_from_the_default_are_reported_with_both() {
        let report = fill("[sandbox]\nbackend = \"podman\"\n").expect("fills");
        let row = report
            .differing
            .iter()
            .find(|(p, _, _)| p == "sandbox.backend")
            .expect("reported");
        assert_eq!(row.1, "\"podman\"", "what the file says");
        assert_eq!(row.2, "\"native\"", "what it would otherwise be");
        // A value equal to the default is not a difference, however it
        // is spelled.
        let same = fill("[sandbox]\nbackend = 'native'\n").expect("fills");
        assert!(
            !same
                .differing
                .iter()
                .any(|(p, _, _)| p == "sandbox.backend"),
            "{:?}",
            same.differing
        );
    }

    #[test]
    fn an_unparseable_file_is_refused_rather_than_guessed_at() {
        let err = fill("[agent\nbroken").expect_err("refused");
        assert!(err.to_string().contains("does not parse"), "{err}");
    }

    #[test]
    fn a_scalar_where_a_table_belongs_is_refused_not_flattened() {
        let err = fill("model = \"openrouter\"\n").expect_err("refused");
        let msg = err.to_string();
        assert!(
            msg.contains("`model` is a value in this config"),
            "the clash is named, not left for the next reader to find: {msg}"
        );
    }

    #[test]
    fn free_form_roots_are_neither_filled_nor_judged() {
        let text = "[platforms.irc]\nrequire_mention = true\n";
        let report = fill(text).expect("fills");
        assert!(report.dead.is_empty(), "{:?}", report.dead);
        assert!(
            !report
                .documented
                .iter()
                .any(|p| p.starts_with("platforms.")),
            "{:?}",
            report.documented
        );
    }

    #[test]
    fn the_element_probe_tells_scalars_from_structs() {
        assert!(
            is_scalar_array("plugins.enabled"),
            "a Vec<String> holds scalars"
        );
        assert!(
            !is_scalar_array("acl.entries"),
            "a Vec<struct> does not, so it must never be written as `= []`"
        );
        assert!(!is_scalar_array("custom_providers"));
        // The lenient fields are the reason the probe asks about the
        // element instead of reading the complaint: `mode` takes a bool or
        // a string, and says so in its own words.
        let all = knobs();
        let shapes: HashMap<&str, Shape> = all.iter().map(|k| (k.path.as_str(), k.shape)).collect();
        assert_eq!(shapes["approvals.mode"], Shape::Value);
        assert_eq!(shapes["sandbox.extra_args"], Shape::ScalarArray);
        assert_eq!(shapes["acl.entries"], Shape::TableArray);
        assert_eq!(shapes["acl.users"], Shape::TableArray);
        assert_eq!(shapes["sandbox.mounts"], Shape::TableArray);
        assert_eq!(shapes["model.provider"], Shape::Unset);
    }

    #[test]
    fn the_same_key_in_two_sections_is_documented_twice() {
        // `model.provider` and `vision.provider` render the same comment
        // line; scoping the duplicate check to the section is what keeps
        // the second one from being swallowed.
        let text = filled();
        for section in ["[model]", "[vision]"] {
            let at = text
                .find(section)
                .unwrap_or_else(|| panic!("{section} exists"));
            let body = &text[at..];
            let body = body.split("\n[").next().unwrap_or(body);
            assert!(
                body.contains("# provider ="),
                "{section} names its own unset provider:\n{text}"
            );
        }
    }

    #[test]
    fn open_maps_hold_names_only_the_user_knows() {
        let open = open_maps();
        for root in [
            "model_aliases",
            "tool_aliases",
            "acl.roles",
            "tools.overrides",
        ] {
            assert!(
                open.iter().any(|m| m == root),
                "{root} is a HashMap: its keys are not a surface. got {open:?}"
            );
        }
        // A struct is not an open map, or every section would escape the
        // audit: its unset Options arrive as nulls, so it is never `{}`.
        assert!(
            !open.iter().any(|m| m == "acl" || m == "sandbox"),
            "{open:?}"
        );
    }

    #[test]
    fn nothing_inside_an_open_map_is_called_dead() {
        let text = "\
[model_aliases.zai]
model = \"glm-4.6\"
provider = \"zai\"

[acl.roles]
\"irc:wrath\" = \"owner\"
";
        let report = fill(text).expect("fills");
        assert!(
            report.dead.is_empty(),
            "the user's own alias and role names are not dead keys: {:?}",
            report.dead
        );
    }

    #[test]
    fn an_array_of_tables_is_reported_as_a_count_not_a_page() {
        let text = "[[fallback_providers]]\nmodel = \"glm-5.3\"\nprovider = \"custom:zai\"\n";
        let report = fill(text).expect("fills");
        let row = report
            .differing
            .iter()
            .find(|(p, _, _)| p == "fallback_providers")
            .expect("reported");
        assert_eq!(row.1, "1 entry", "a count, not the rendered list");
        assert_eq!(row.2, "none declared");
        assert!(
            report.differing.iter().all(|(_, got, _)| got.len() < 40),
            "no row is a page of inline tables: {:?}",
            report.differing
        );
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    /// The crate's own source, minus each file's test module: a test that
    /// sets a var is not a reader of it.
    pub(super) fn source() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
            for entry in std::fs::read_dir(dir).expect("src is readable") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(&path, out);
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let raw = std::fs::read_to_string(&path).expect("reads");
                out.push((path.display().to_string(), raw));
            }
        }
        let mut out = Vec::new();
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut out,
        );
        // The surfaces are their own crate since 2026-09-28 (veles-frontends);
        // they are engine sources too.
        walk(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../veles-frontends/src"),
            &mut out,
        );
        assert!(out.len() > 30, "the walk found nothing: {}", out.len());
        out
    }

    /// Every ALL_CAPS literal on a line that reads through the dotenv.
    ///
    /// The shapes are the readers: `env_or_dotenv(` covers authn's and
    /// both of providers'; `secrets::resolve(` is the resolver they all
    /// go through, called directly; `resolve_bind_host(` and `from_env(`
    /// take the name as an argument; `_KEY: &str =` is how authn names
    /// the desktop and peer keys it then resolves; and `VELES_API_KEY`
    /// is the default `key_env` that `resolve_api_key` falls back to.
    fn scanned_vars() -> Vec<String> {
        let shapes = [
            "env_or_dotenv(",
            "secrets::resolve(",
            "resolve_bind_host(",
            "from_env(",
            "_KEY: &str =",
            "\"VELES_API_KEY\"",
        ];
        let mut out = Vec::new();
        for (_, raw) in source() {
            // A test that sets a var is not a reader of it, so each
            // file is scanned only up to its first test module.
            let body = match raw.find("#[cfg(test)]") {
                Some(i) => &raw[..i],
                None => raw.as_str(),
            };
            for line in body.lines() {
                if !shapes.iter().any(|s| line.contains(s)) {
                    continue;
                }
                let mut rest = line;
                while let Some(i) = rest.find('"') {
                    let after = &rest[i + 1..];
                    let Some(j) = after.find('"') else { break };
                    let lit = &after[..j];
                    if lit.len() > 3
                        && lit
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    {
                        out.push(lit.to_string());
                    }
                    rest = &after[j..];
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    /// `VELES_DESKTOP_OPEN` is declared beside the two desktop token keys,
    /// and read with a plain `std::env::var` — an escape hatch from minting
    /// a token, not a configuration knob, so it is deliberately not
    /// documented as one. `VELES_BUILD_KEY` is no runtime variable at all:
    /// module.rs's `BUILD_KEY` is `env!` of what build.rs baked.
    const EXEMPT: [&str; 2] = ["VELES_DESKTOP_OPEN", "VELES_BUILD_KEY"];

    #[test]
    fn every_listed_var_is_really_read_from_dotenv() {
        let scanned = scanned_vars();
        for knob in env_knobs() {
            assert!(
                scanned.contains(&knob.name),
                "{} is documented for .env but no dotenv-aware reader in core reads it",
                knob.name
            );
        }
    }

    #[test]
    fn the_scan_finds_no_var_the_list_forgets() {
        let listed: Vec<String> = env_knobs().into_iter().map(|k| k.name).collect();
        for var in scanned_vars() {
            if EXEMPT.contains(&var.as_str()) {
                continue;
            }
            assert!(
                listed.contains(&var),
                "{var} is read through the dotenv but not documented"
            );
        }
    }

    #[test]
    fn the_exempt_var_is_really_read_without_the_dotenv() {
        let (_, body) = source()
            .into_iter()
            .find(|(p, _)| p.ends_with("authn.rs"))
            .expect("authn.rs");
        assert!(
            body.contains("std::env::var(DESKTOP_OPEN_KEY)"),
            "VELES_DESKTOP_OPEN is exempt because it is read straight from \
             the environment; if that changed it belongs on the list"
        );
    }

    #[test]
    fn the_user_agent_default_is_the_one_the_code_falls_back_to() {
        let listed = env_knobs()
            .into_iter()
            .find(|k| k.name == "VELES_USER_AGENT")
            .expect("listed");
        assert_eq!(
            listed.default.as_deref(),
            Some(
                crate::providers::UserAgentProfile::veles()
                    .user_agent
                    .as_str()
            ),
            "the documented fallback and the real one disagree"
        );
    }

    #[test]
    fn the_a2a_port_default_is_the_config_default() {
        let listed = env_knobs()
            .into_iter()
            .find(|k| k.name == "A2A_PORT")
            .expect("listed")
            .default
            .expect("has a default");
        let in_config = platform_surface()
            .into_iter()
            .find(|k| k.path == "platforms.a2a.extra.port")
            .expect("listed")
            .default
            .expect("has a default");
        assert_eq!(
            listed, in_config,
            "A2A_PORT falls back to platforms.a2a.extra.port, so the two \
             documented defaults are one fact"
        );
    }

    /// `A2A_HOST` moved from `.env`-only to the config key
    /// `platforms.a2a.extra.host` (2026-10-20). Both halves have to show:
    /// `veles config` names the key with the var that beats it, and the env
    /// list says the var falls back to the key, as `A2A_PORT` does.
    #[test]
    fn the_a2a_host_is_a_config_key_the_env_var_beats() {
        let knob = platform_surface()
            .into_iter()
            .find(|k| k.path == "platforms.a2a.extra.host")
            .expect("platforms.a2a.extra.host is on the surface");
        assert_eq!(knob.env.as_deref(), Some("A2A_HOST"));
        assert_eq!(knob.default, None, "unset means loopback");
        assert!(!knob.writable(), "a fill names it, never writes it");
        let var = env_knobs()
            .into_iter()
            .find(|k| k.name == "A2A_HOST")
            .expect("A2A_HOST is still read through the dotenv, so still listed");
        assert!(
            var.note.contains("platforms.a2a.extra.host"),
            "the env note names the key it beats: {}",
            var.note
        );
    }

    #[test]
    fn no_var_is_listed_twice() {
        let mut names: Vec<String> = env_knobs().into_iter().map(|k| k.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "a var is documented twice");
    }

    #[test]
    fn a_live_value_is_never_touched() {
        let text = "\
# the gateway's own port
A2A_PORT=9911
TELEGRAM_BOT_TOKEN=123:secret
";
        let out = env_fill(text, &[]);
        assert!(
            out.text.starts_with(text),
            "a fill rewrote what was already there:\n{}",
            out.text
        );
        // `live` is about the listed surface: TELEGRAM_BOT_TOKEN belongs
        // to the telegram crate's, so it is left alone but not reported.
        assert_eq!(out.live, vec!["A2A_PORT"]);
        assert!(
            !out.added.iter().any(|n| n == "A2A_PORT"),
            "a live port was documented again: {:?}",
            out.added
        );
        assert!(
            out.text.contains("TELEGRAM_BOT_TOKEN=123:secret"),
            "a credential that is not on the list was touched"
        );
    }

    #[test]
    fn a_second_fill_adds_nothing() {
        let first = env_fill("", &[]);
        assert!(!first.added.is_empty(), "the first fill documents the list");
        let second = env_fill(&first.text, &[]);
        assert!(second.is_noop(), "a second fill added {:?}", second.added);
        assert_eq!(second.text, first.text, "the file changed anyway");
    }

    /// The whole point of two lines per var: uncommenting the assignment
    /// has to produce a value a reader can parse. A trailing comment would
    /// make `A2A_PORT` the string `9900  # the port`, which fails to parse
    /// and falls back — silently, with the file looking configured.
    #[test]
    fn uncommenting_a_documented_line_assigns_a_clean_value() {
        let out = env_fill("", &[]);
        let mut assignments = 0;
        for line in out.text.lines() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            assert!(t.starts_with('#'), "a fill wrote an uncommented line: {t}");
            let Some((name, _)) = assigned(t) else {
                continue;
            };
            assignments += 1;
            assert!(
                !value_contains_comment(t),
                "{name}: the value carries a comment, so uncommenting it assigns junk"
            );
        }
        assert!(assignments > 10, "nothing was documented: {assignments}");
    }

    /// The value part of an assignment line, after the `#`s are off.
    fn value_contains_comment(line: &str) -> bool {
        let t = line.trim_start_matches('#').trim();
        let Some((_, value)) = t.split_once('=') else {
            return false;
        };
        value.contains('#') || value.trim_end() != value
    }

    /// Documenting a var must not change what the env-file reader
    /// resolves: it skips `#`-prefixed lines, and this is the test that
    /// says so out loud. Both texts go through `secrets::parse_env`, the
    /// reader `resolve_in` applies to `.env`.
    ///
    /// The FILE reader only. This used to go through the whole resolver,
    /// which also consults the process environment and the vault, so the
    /// answer depended on what else the test process had loaded: on the
    /// Windows check a live bot token turned up in one of the two
    /// resolutions and not the other, and the failure message printed it
    /// (2026-10-29). A failure now names the variables, never their values.
    #[test]
    fn the_documented_lines_resolve_to_nothing() {
        let live = "A2A_PORT=9911\n";
        let filled = env_fill(live, &[]).text;

        let mut names: Vec<String> = env_knobs().into_iter().map(|k| k.name).collect();
        names.extend(["IRC_SERVER", "TELEGRAM_BOT_TOKEN"].map(str::to_string));

        let resolve = |text: &str| -> Vec<(String, Option<String>)> {
            let parsed = crate::secrets::parse_env(text);
            names
                .iter()
                .map(|n| {
                    let v = parsed
                        .iter()
                        .find(|(k, v)| k == n && !v.is_empty())
                        .map(|(_, v)| v.clone());
                    (n.clone(), v)
                })
                .collect()
        };
        let before = resolve(live);
        let after = resolve(&filled);
        let changed: Vec<&str> = before
            .iter()
            .zip(&after)
            .filter(|(b, a)| b != a)
            .map(|(b, _)| b.0.as_str())
            .collect();
        assert!(
            changed.is_empty(),
            "documenting the vars changed what the reader resolves for {changed:?}"
        );
        // The comparison above would also pass if nothing were read at
        // all, so the live value has to arrive from the file.
        let port = before
            .iter()
            .find(|(n, _)| n == "A2A_PORT")
            .expect("listed");
        assert_eq!(
            port.1.as_deref(),
            Some("9911"),
            "the file's live value was not read at all"
        );
    }

    #[test]
    fn the_vars_that_choose_the_file_are_named() {
        let out = env_fill("VELES_HOME=/srv/veles\nXDG_DATA_HOME=/var/lib/x\n", &[]);
        let names: Vec<String> = out.misplaced.iter().map(|(n, _)| n.clone()).collect();
        assert_eq!(names, vec!["VELES_HOME", "XDG_DATA_HOME"], "{names:?}");

        // The claim behind the verdict: `load()` has already picked the
        // directory by the time it reads the file in it.
        let (_, body) = source()
            .into_iter()
            .find(|(p, _)| p.ends_with("config.rs"))
            .expect("config.rs");
        let start = body.find("pub fn load()").expect("load() is there");
        let load_body = &body[start..];
        let chosen = load_body.find("config_dir()").expect("locates the dir");
        let read = load_body.find("load_env_file(").expect("then reads .env");
        assert!(
            chosen < read,
            "load() reads .env before choosing where it is; the verdict is wrong"
        );
        for (name, _) in chosen_before_the_file() {
            assert!(
                body.contains(&format!("\"{name}\"")),
                "{name} is named as a bootstrap var but config.rs never reads it"
            );
        }
    }
}

#[cfg(test)]
mod env_shape_tests {
    use super::*;

    /// A comment that mentions a var in prose is not an assignment. Count
    /// it as documenting the var and a fill would skip a knob the file has
    /// never actually set — the silence this whole surface exists to end.
    #[test]
    fn prose_that_mentions_a_var_is_not_an_assignment() {
        let text = "# I tried A2A_RATE_LIMIT=30 once and it was too low\n";
        let out = env_fill(text, &[]);
        assert!(
            out.added.iter().any(|n| n == "A2A_RATE_LIMIT"),
            "a note about the var counted as setting it: {:?}",
            out.added
        );
        assert!(out.live.is_empty(), "prose is not a live value");
        assert!(
            out.text.starts_with(text),
            "the original comment was rewritten"
        );
    }

    #[test]
    fn the_appended_block_is_separated_from_what_was_there() {
        // No trailing newline: the common shape of a hand-edited file.
        let out = env_fill("A2A_PORT=9911", &[]);
        assert!(
            out.text.contains("\n\n# Vars this build of veles reads"),
            "the block was glued to the last line:\n{}",
            out.text
        );
        assert!(
            out.text.starts_with("A2A_PORT=9911\n"),
            "the live line was altered: {:?}",
            out.text.lines().next()
        );
    }

    /// A documented fallback is a claim about the reader. `A2A_PORT` and
    /// `VELES_USER_AGENT` are pinned against theirs; this pins the one
    /// that is a bare number in a `unwrap_or`, where a stale default would
    /// be hardest to notice.
    #[test]
    fn the_pingpong_default_is_the_one_in_the_reader() {
        let (_, body) = super::env_tests::source()
            .into_iter()
            .find(|(p, _)| p.ends_with("a2a.rs"))
            .expect("a2a.rs");
        let start = body.find("fn max_turns(").expect("max_turns is there");
        let body = &body[start..];
        let end = body.find('}').expect("closes");
        let body = &body[..end];
        let fallback = body
            .split("unwrap_or(")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .expect("max_turns falls back to a literal");
        let listed = env_knobs()
            .into_iter()
            .find(|k| k.name == "A2A_MAX_PINGPONG_TURNS")
            .expect("listed")
            .default
            .expect("has a default");
        assert_eq!(
            listed,
            fallback.trim(),
            "the documented fallback and max_turns()'s disagree"
        );
    }
}

#[cfg(test)]
mod differing_tests {
    use super::*;

    /// The posture line of a whole installation. `default_role`'s default
    /// is unset, so there is no value to compare against — and an audit
    /// that skipped it for that reason would hide the one key that decides
    /// what a stranger gets.
    #[test]
    fn an_option_whose_default_is_unset_is_still_a_difference() {
        let report = fill("[acl]\ndefault_role = \"user\"\n").expect("fills");
        let hit = report
            .differing
            .iter()
            .find(|(p, _, _)| p == "acl.default_role")
            .unwrap_or_else(|| panic!("not reported: {:?}", report.differing));
        assert_eq!(hit.1, "\"user\"", "what the file says");
        assert_eq!(hit.2, "unset by default", "what the build falls back to");
    }

    /// And the inverse: leaving it out is not a difference, because unset
    /// IS the default. Reporting it would put every unconfigured option on
    /// the list and bury the ones somebody chose.
    #[test]
    fn an_option_nobody_set_is_not_a_difference() {
        let report = fill("[acl]\nlockdown = false\n").expect("fills");
        assert!(
            !report
                .differing
                .iter()
                .any(|(p, _, _)| p == "acl.default_role"),
            "an absent key was reported as differing: {:?}",
            report.differing
        );
        assert!(
            !report.differing.iter().any(|(p, _, _)| p == "acl.lockdown"),
            "lockdown = false IS the default: {:?}",
            report.differing
        );
    }

    /// The dead-key footnote must not invent a consumer.
    ///
    /// The CLI's version ended with "the desktop reads some of them" while
    /// every key printed above it was unread by Rust, Tcl and JS alike —
    /// that sentence is why eleven of them survived. So the note is pinned
    /// here, in the crate that decides what "dead" means: it states that
    /// nothing reads them, and claims no reader of its own.
    ///
    /// Mutation: putting a frontend back in the sentence makes this fail.
    #[test]
    fn the_dead_key_note_claims_no_reader() {
        assert_eq!(
            Fill::default().dead_key_note(),
            None,
            "no dead keys, no note"
        );

        // One of the eleven the live audit lists.
        let report = fill("[security]\nredact_secrets = true\n").expect("fills");
        assert!(
            report.dead.contains(&"security.redact_secrets".to_string()),
            "the fixture key should be dead: {:?}",
            report.dead
        );
        let note = report.dead_key_note().expect("a dead key gets a note");
        assert!(
            note.contains("nothing in this build reads"),
            "it must say what the list already found: {note}"
        );
        assert!(
            note.contains("operator's call"),
            "and whose decision deleting them is: {note}"
        );
        for lie in ["desktop reads", "frontend reads", "some of them"] {
            assert!(
                !note.contains(lie),
                "the note must not invent a consumer ({lie:?}): {note}"
            );
        }
    }

    /// The 2026-09-10 tail pinned rather than fixed: CRLF files fill
    /// fine (toml_edit parses them; the set-value grafts preserve the
    /// user's trivia). A file that does NOT parse is named, not
    /// silently half-filled.
    #[test]
    fn a_crlf_file_fills_without_mangling() {
        let text = "[platforms.irc]\r\nserver = \"irc.libera.chat\"\r\n";
        let report = fill_with(text, &[]).expect("CRLF parses");
        assert!(
            report.text.contains("[platforms.irc]\n")
                || report.text.contains("[platforms.irc]\r\n"),
            "the block survives: {}",
            report.text
        );
        // The graft must keep the user's value, not re-write it.
        let parsed: VelesConfig = toml::from_str(&report.text).expect("still parses");
        let v = serde_json::to_value(parsed).unwrap();
        assert_eq!(
            v["platforms"]["irc"]["server"].as_str(),
            Some("irc.libera.chat"),
            "the user's server survives a fill: {v}"
        );
    }

    /// A split table (the same `[model]` twice) is a TOML parse
    /// error, and `fill_with` refuses to guess: the config is named as
    /// unreadable, not half-filled out from under the duplicate.
    #[test]
    fn a_duplicate_table_is_refused_by_name_not_half_filled() {
        let err = fill_with("[model]\nprovider = \"a\"\n[model]\ndefault = \"b\"\n", &[]);
        let e = format!("{}", err.expect_err("a split table does not parse"));
        assert!(
            e.contains("does not parse"),
            "the error says the file is unreadable, not a lie about a key: {e}"
        );
    }
}
