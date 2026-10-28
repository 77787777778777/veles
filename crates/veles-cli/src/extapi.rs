//! The extension API's published spec (PLAN-extension-api.md D1, D7).
//!
//! # Why this exists
//!
//! A plugin in `~/.config/veles/plugins/` survives every update of
//! veles, which is the point of the stable tiers: an operator changes
//! behaviour there instead of patching our tree and re-merging it each
//! release (USER 2026-10-11, the hermes failover patch). That only holds
//! while the surface the plugin calls stays put, and until now nothing
//! said what that surface was, so nothing could notice it moving.
//!
//! [`spec`] reads it from the code: the bridge op table, the hook names,
//! the plugin.toml and manifest keys, the four script bindings, the
//! process protocol's method names and the C ABI's header. Where a
//! constant exists it is read directly; where the only statement of the
//! surface is a source file a plugin author reads (the Tcl stub, the JS
//! prelude, the Racket and Lisp packages, the ABI crate), that file is
//! parsed, because it IS the contract.
//!
//! The spec is checked in as `docs/extension-api.json`. Three checks keep
//! it, the changelog and the author doc honest, and the gate runs them
//! (`crates/veles-cli/tests/ext_api.rs`):
//!
//! - [`check_spec`]: the file equals what the code says now;
//! - [`check_changelog`]: the newest `docs/extension-api-changes.md`
//!   entry names this version and this file's hash, and versions only
//!   go up, so a change cannot ship without an entry;
//! - [`check_docs`]: `docs/extending.md` carries the same stamp and
//!   names every item, and its reference tables name nothing the spec
//!   does not have.
//!
//! The docs site (`~/veles-docs-work`) is checked by
//! `scripts/check-ext-api-site.sh` in the precommit gate.
//!
//! # What is not in it
//!
//! The module tier (a Rust dylib calling `veles_core` directly, the
//! layer traits) and overlay modules: both are same-build only and make
//! no stability promise.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use veles_runtime::veles_core;
use veles_runtime::veles_types;
use veles_types::plugin::{
    HookDecl, Instances, Manifest, PlatformDecl, SecretDecl, ServiceDecl, UseDecl,
};

/// The spec file, relative to the repository root.
pub const SPEC_PATH: &str = "docs/extension-api.json";
/// The changelog, relative to the repository root.
pub const CHANGES_PATH: &str = "docs/extension-api-changes.md";
/// The author doc, relative to the repository root.
pub const DOC_PATH: &str = "docs/extending.md";

/// The stamp line's prefix in the author doc.
const STAMP_PREFIX: &str = "<!-- extension-api: ";
/// The reference section's markers in the author doc.
const REF_BEGIN: &str = "<!-- extension-api:reference:begin -->";
const REF_END: &str = "<!-- extension-api:reference:end -->";

// The files whose text IS the contract a plugin author reads.
const TCL_STUB: &str = include_str!("../../../plugins/veles_api.tcl");
const JS_PRELUDE: &str = include_str!("../../veles-lang-js/src/prelude.js");
const RACKET_API: &str = include_str!("../../veles-lang-racket/src/veles.rkt");
const LISP_API: &str = include_str!("../../veles-lang-lisp/src/veles.lisp");
const ABI_SRC: &str = include_str!("../../veles-plugin-abi/src/lib.rs");
const PROTOCOL_SRC: &str = include_str!("../../veles-types/src/plugin.rs");

/// Plugin kinds a `plugin.toml` may name. The script kinds are the
/// script seam's own list; the other four are the loaders' literals.
fn plugin_kinds() -> Vec<String> {
    let mut kinds = vec!["tcl".to_string(), "rust".to_string()];
    kinds.extend(
        veles_core::scripthost::SCRIPT_KINDS
            .iter()
            .map(|k| k.to_string()),
    );
    kinds.push("module".into());
    kinds.push("process".into());
    kinds
}

// ── the spec ─────────────────────────────────────────────────────────

/// The extension API as the code states it now.
pub fn spec() -> Value {
    let tcl = tcl_procs(TCL_STUB);
    let js = js_bindings(JS_PRELUDE);
    let racket = paren_list_after(RACKET_API, "(provide");
    let lisp: Vec<String> = paren_list_after(LISP_API, "(:export")
        .into_iter()
        .map(|s| s.trim_start_matches("#:").to_string())
        .collect();

    let ops: Vec<Value> = veles_core::bridgeops::OP_NAMES
        .iter()
        .map(|op| {
            json!({
                "name": op,
                "tcl": tcl.iter().find(|(n, _)| n == op).map(|(n, a)| tcl_signature(n, a)),
                "js": present(&js, &js_name(op)),
                "racket": present(&racket, &kebab(op)),
                "lisp": present(&lisp, &kebab(op)),
            })
        })
        .collect();

    let script_registrars: Vec<Value> = veles_core::bridgeops::TCL_ONLY_COMMANDS
        .iter()
        .copied()
        .chain(std::iter::once("hook"))
        .map(|r| {
            json!({
                "name": r,
                "tcl": tcl.iter().find(|(n, _)| n == r).map(|(n, a)| tcl_signature(n, a)),
                "js": present(&js, &js_name(r)),
                "racket": present(&racket, &kebab(r)),
                "lisp": present(&lisp, &kebab(r)),
            })
        })
        .collect();

    let instances: Vec<Value> = [
        Instances::Single,
        Instances::PerEntry {
            array: String::new(),
            name: String::new(),
        },
    ]
    .iter()
    .map(|i| {
        // Exhaustive on purpose: a new variant does not compile until
        // it is named here, and so reaches the spec.
        match i {
            Instances::Single | Instances::PerEntry { .. } => {}
        }
        let v = serde_json::to_value(i).unwrap_or(Value::Null);
        let mut keys: Vec<String> = v
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        keys.sort();
        json!({ "kind": v["kind"], "keys": keys })
    })
    .collect();

    let (abi_consts, abi_types, abi_structs) = abi_items(ABI_SRC);

    canonical(json!({
        "api_version": veles_types::plugin::EXT_API_VERSION,
        "generated_by": "veles plugins api --write (do not edit by hand)",
        "ops": ops,
        "script_registrars": script_registrars,
        "bindings": {
            "tcl": tcl.iter().map(|(n, a)| tcl_signature(n, a)).collect::<Vec<_>>(),
            "js": js,
            "racket": racket,
            "lisp": lisp,
        },
        "hooks": {
            "fired": veles_core::hooks::FIRED_HOOKS,
            "reserved": veles_core::hooks::RESERVED_HOOKS,
        },
        "plugin_toml": {
            "keys": sorted(
                struct_fields::<veles_core::plugins::PluginManifest>()
                    .into_iter()
                    .chain(struct_fields::<veles_core::plugins::ServiceDeclarations>())
            ),
            "kinds": plugin_kinds(),
        },
        "manifest": {
            "Manifest": struct_fields::<Manifest>(),
            "ServiceDecl": struct_fields::<ServiceDecl>(),
            "UseDecl": struct_fields::<UseDecl>(),
            "HookDecl": struct_fields::<HookDecl>(),
            "PlatformDecl": struct_fields::<PlatformDecl>(),
            "SecretDecl": struct_fields::<SecretDecl>(),
            "PointDecl": struct_fields::<veles_types::plugin::PointDecl>(),
            "OnError": struct_fields::<veles_types::plugin::OnError>(),
            "Instances": instances,
            "service_cap_any": veles_types::plugin::SERVICE_CAP_ANY,
        },
        "process_protocol": {
            "version": veles_types::plugin::PROTOCOL,
            "methods": protocol_methods(PROTOCOL_SRC)
                .into_iter()
                .map(|(c, v)| json!({ "const": c, "name": v }))
                .collect::<Vec<_>>(),
        },
        "abi": {
            "consts": abi_consts,
            "types": abi_types,
            "structs": abi_structs,
        },
        "override_points": veles_core::services::core_point_decls()
            .into_iter()
            // The in-tree modules' own points, from their manifests.
            .chain(veles_irc::module::manifest().points)
            .chain(veles_telegram::module::manifest().points)
            .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
            .collect::<Vec<_>>(),
    }))
}

/// The spec as the checked-in file holds it: pretty JSON, keys sorted,
/// one trailing newline.
pub fn spec_text() -> String {
    let mut s = serde_json::to_string_pretty(&spec()).unwrap_or_default();
    s.push('\n');
    s
}

/// `<version> sha256:<first 16 hex>` of a spec file's text: what the
/// changelog heading, the author doc and the site page carry.
pub fn stamp_of(spec_text: &str) -> String {
    let version = serde_json::from_str::<Value>(spec_text)
        .ok()
        .and_then(|v| v["api_version"].as_str().map(String::from))
        .unwrap_or_else(|| "?".into());
    format!("{version} sha256:{}", short_hash(spec_text))
}

fn short_hash(text: &str) -> String {
    let full = format!("{:x}", Sha256::digest(text.as_bytes()));
    full[..16].to_string()
}

/// Every name the author doc must mention: ops, every binding in every
/// language, script registrars, hooks, plugin.toml keys and kinds,
/// manifest keys, protocol methods, the ABI's exported symbols, structs
/// and host-table fields, and the override points.
pub fn doc_names(spec: &Value) -> BTreeSet<String> {
    fn add_strs(out: &mut BTreeSet<String>, v: &Value) {
        for s in v.as_array().into_iter().flatten() {
            if let Some(s) = s.as_str() {
                out.insert(s.to_string());
            }
        }
    }
    let mut out = BTreeSet::new();
    add_strs(&mut out, &spec["bindings"]["js"]);
    add_strs(&mut out, &spec["bindings"]["racket"]);
    add_strs(&mut out, &spec["bindings"]["lisp"]);
    add_strs(&mut out, &spec["hooks"]["fired"]);
    add_strs(&mut out, &spec["hooks"]["reserved"]);
    add_strs(&mut out, &spec["plugin_toml"]["keys"]);
    add_strs(&mut out, &spec["plugin_toml"]["kinds"]);
    for (k, v) in spec["manifest"].as_object().into_iter().flatten() {
        if k == "Instances" {
            for i in v.as_array().into_iter().flatten() {
                if let Some(kind) = i["kind"].as_str() {
                    out.insert(kind.to_string());
                }
            }
        } else if v.is_array() {
            add_strs(&mut out, v);
        }
    }
    for b in spec["bindings"]["tcl"].as_array().into_iter().flatten() {
        if let Some(name) = b.as_str().and_then(|s| s.split_whitespace().next()) {
            out.insert(name.trim_start_matches("::veles::").to_string());
        }
    }
    for list in ["ops", "script_registrars"] {
        for o in spec[list].as_array().into_iter().flatten() {
            if let Some(n) = o["name"].as_str() {
                out.insert(n.to_string());
            }
        }
    }
    for m in spec["process_protocol"]["methods"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(n) = m["name"].as_str() {
            out.insert(n.to_string());
        }
    }
    for c in spec["abi"]["consts"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or("");
        if name.starts_with("SYMBOL_") {
            // The symbol itself is what an author exports.
            if let Some(sym) = c["value"]
                .as_str()
                .and_then(|v| v.strip_prefix("b\""))
                .and_then(|v| v.strip_suffix("\\0\""))
            {
                out.insert(sym.to_string());
            }
        }
    }
    for s in spec["abi"]["structs"].as_array().into_iter().flatten() {
        let name = s["name"].as_str().unwrap_or("");
        out.insert(name.to_string());
        if name == "VelesHostApi" {
            for f in s["fields"].as_array().into_iter().flatten() {
                if let Some(n) = f["name"].as_str() {
                    out.insert(n.to_string());
                }
            }
        }
    }
    for p in spec["override_points"].as_array().into_iter().flatten() {
        if let Some(n) = p["name"].as_str() {
            out.insert(n.to_string());
        }
    }
    out.remove("");
    out
}

// ── `veles plugins api` ──────────────────────────────────────────────

/// What `veles plugins api` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiCmd {
    /// A summary: the version, the stamp, the size of each surface.
    Summary,
    /// The spec as JSON (what `--write` would write).
    Json,
    /// Write `docs/extension-api.json` in the current directory, which
    /// must be the repository root.
    Write,
    /// The changelog heading for the spec as the code states it now.
    Stamp,
    /// The three drift checks, against the current directory when it is
    /// a checkout, else the tree this binary was built from.
    Check,
}

pub fn cli(cmd: ApiCmd) -> anyhow::Result<()> {
    let text = spec_text();
    match cmd {
        ApiCmd::Json => print!("{text}"),
        ApiCmd::Stamp => println!(
            "## {} ({})",
            stamp_of(&text),
            chrono::Local::now().format("%Y-%m-%d")
        ),
        ApiCmd::Write => {
            let here = std::env::current_dir()?;
            if !here.join("docs").is_dir() || !here.join("crates/veles-cli").is_dir() {
                anyhow::bail!(
                    "run this in the veles repository root (no docs/ and crates/veles-cli here)"
                );
            }
            std::fs::write(here.join(SPEC_PATH), &text)?;
            println!("wrote {SPEC_PATH}: {}", stamp_of(&text));
            println!(
                "next: a `## {}` entry at the top of {CHANGES_PATH},",
                stamp_of(&text)
            );
            println!("then {DOC_PATH} (its text and stamp) and the docs site page");
        }
        ApiCmd::Check => {
            let here = std::env::current_dir()?;
            let root = if here.join(SPEC_PATH).is_file() {
                here
            } else {
                repo_root()
            };
            let results = [
                ("spec matches the code", check_spec(&root)),
                ("changelog names the spec", check_changelog(&root)),
                ("docs match the spec", check_docs(&root)),
            ];
            let mut failed = false;
            for (what, r) in &results {
                match r {
                    Ok(()) => println!("OK    {what}"),
                    Err(e) => {
                        failed = true;
                        println!("FAIL  {what}\n{e}\n");
                    }
                }
            }
            if failed {
                anyhow::bail!("the extension API has drifted (above)");
            }
        }
        ApiCmd::Summary => {
            let s = spec();
            let n = |v: &Value| v.as_array().map(Vec::len).unwrap_or(0);
            println!("extension API {}", stamp_of(&text));
            println!(
                "  {} ops, {} script registrars; bindings: tcl {}, js {}, racket {}, lisp {}",
                n(&s["ops"]),
                n(&s["script_registrars"]),
                n(&s["bindings"]["tcl"]),
                n(&s["bindings"]["js"]),
                n(&s["bindings"]["racket"]),
                n(&s["bindings"]["lisp"]),
            );
            println!(
                "  {} fired + {} reserved hooks; {} plugin.toml keys, {} kinds",
                n(&s["hooks"]["fired"]),
                n(&s["hooks"]["reserved"]),
                n(&s["plugin_toml"]["keys"]),
                n(&s["plugin_toml"]["kinds"]),
            );
            println!(
                "  process protocol v{} ({} methods and service names); C ABI: {} constants, {} types, {} structs",
                s["process_protocol"]["version"],
                n(&s["process_protocol"]["methods"]),
                n(&s["abi"]["consts"]),
                n(&s["abi"]["types"]),
                n(&s["abi"]["structs"]),
            );
            println!("  {} override points", n(&s["override_points"]));
            println!(
                "published as {SPEC_PATH}; `--check` compares, `--json` prints, `--write` rewrites"
            );
        }
    }
    Ok(())
}

// ── the checks ───────────────────────────────────────────────────────

/// The repository root this binary was built from, for the checks.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `docs/<name>` under the repository root. Spelled `repo.join("docs")`
/// on purpose: precommit's "fileset covers test paths" step greps for
/// that form and holds flake.nix's rustSrc to the directory, so the
/// checks cannot pass vacuously in the nix sandbox.
fn read(repo: &Path, rel: &str) -> Result<String, String> {
    let name = rel.strip_prefix("docs/").unwrap_or(rel);
    std::fs::read_to_string(repo.join("docs").join(name)).map_err(|e| format!("{rel}: {e}"))
}

const FIX: &str = "The extension API changed. To publish the change (docs/extending.md, \
\"How a change to the API is published\"):\n\
  1. bump EXT_API_VERSION in crates/veles-types/src/plugin.rs;\n\
  2. `veles plugins api --write` (in the repo root) rewrites docs/extension-api.json;\n\
  3. add an entry at the TOP of docs/extension-api-changes.md headed with what\n\
     `veles plugins api --stamp` prints: what changed, what an author must do;\n\
  4. update docs/extending.md (whatever the change touches, then its stamp line);\n\
  5. update the docs site page (~/veles-docs-work/src/en/extending.md) and run\n\
     build/refresh-ext-api.sh there, which prints the stamp the page needs.";

/// The checked-in spec equals what the code says now.
pub fn check_spec(root: &Path) -> Result<(), String> {
    let have = read(root, SPEC_PATH)?;
    let want = spec_text();
    if have == want {
        return Ok(());
    }
    Err(format!(
        "{SPEC_PATH} does not match the code.\n{}\n{FIX}",
        line_diff(&have, &want)
    ))
}

/// One changelog entry heading: `## <version> sha256:<hash16> …`.
fn changelog_entries(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.strip_prefix("## "))
        .filter_map(|h| {
            let mut it = h.split_whitespace();
            let version = it.next()?.to_string();
            let hash = it.next()?.strip_prefix("sha256:")?.to_string();
            Some((version, hash))
        })
        .collect()
}

fn parse_version(v: &str) -> Option<(u32, u32)> {
    let (a, b) = v.split_once('.')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// The newest changelog entry names this spec's version and hash, and
/// versions strictly decrease down the file.
pub fn check_changelog(root: &Path) -> Result<(), String> {
    let spec_text = read(root, SPEC_PATH)?;
    let changes = read(root, CHANGES_PATH)?;
    let entries = changelog_entries(&changes);
    let stamp = stamp_of(&spec_text);
    let Some((version, hash)) = entries.first() else {
        return Err(format!(
            "{CHANGES_PATH} has no `## <version> sha256:<hash>` entry; the newest must be `## {stamp}`.\n{FIX}"
        ));
    };
    let spec_version = stamp.split_whitespace().next().unwrap_or("?");
    if version != spec_version || *hash != short_hash(&spec_text) {
        return Err(format!(
            "the newest {CHANGES_PATH} entry is `## {version} sha256:{hash}`, the spec is `{stamp}`.\n{FIX}"
        ));
    }
    let mut prev: Option<(u32, u32)> = None;
    for (v, _) in &entries {
        let Some(p) = parse_version(v) else {
            return Err(format!(
                "{CHANGES_PATH}: `{v}` is not a <major>.<minor> version"
            ));
        };
        if let Some(q) = prev {
            if p >= q {
                return Err(format!(
                    "{CHANGES_PATH}: versions must go DOWN the file (newest first, each once): {}.{} after {}.{}",
                    p.0, p.1, q.0, q.1
                ));
            }
        }
        prev = Some(p);
    }
    Ok(())
}

/// `docs/extending.md` carries the spec's stamp, names every item, and
/// its reference tables name nothing the spec lacks.
pub fn check_docs(root: &Path) -> Result<(), String> {
    let spec_text = read(root, SPEC_PATH)?;
    let doc = read(root, DOC_PATH)?;
    let stamp = stamp_of(&spec_text);
    let found = doc
        .lines()
        .find_map(|l| l.trim().strip_prefix(STAMP_PREFIX))
        .and_then(|l| l.strip_suffix("-->"))
        .map(|s| s.trim().to_string());
    let mut problems = Vec::new();
    match found {
        Some(s) if s == stamp => {}
        Some(s) => problems.push(format!(
            "its stamp says `{s}`, the spec is `{stamp}`: read the change, update the text, then the stamp"
        )),
        None => problems.push(format!("no stamp line `{STAMP_PREFIX}{stamp} -->`")),
    }
    let spec: Value = serde_json::from_str(&spec_text).map_err(|e| format!("{SPEC_PATH}: {e}"))?;
    let names = doc_names(&spec);
    let mentioned = backticked(&doc);
    let missing: Vec<&String> = names.iter().filter(|n| !mentioned.contains(*n)).collect();
    if !missing.is_empty() {
        problems.push(format!(
            "it never names (in backticks) {} item(s) of the spec: {}",
            missing.len(),
            missing
                .iter()
                .map(|s| format!("`{s}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    match reference_section(&doc) {
        None => problems.push(format!("no reference section ({REF_BEGIN} … {REF_END})")),
        Some(section) => {
            let stale: Vec<String> = first_cells(section)
                .into_iter()
                .filter(|n| !names.contains(n))
                .collect();
            if !stale.is_empty() {
                problems.push(format!(
                    "its reference tables name what the spec does not have: {}",
                    stale
                        .iter()
                        .map(|s| format!("`{s}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{DOC_PATH} has drifted from the spec:\n- {}\n{FIX}",
            problems.join("\n- ")
        ))
    }
}

/// Every `code span` in a markdown text, as written.
fn backticked(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            let Some(end) = after.find('`') else { break };
            let span = &after[..end];
            out.insert(span.trim().to_string());
            // A signature span names its proc first: `::veles::x a b`.
            if let Some(first) = span.split_whitespace().next() {
                out.insert(first.trim_start_matches("::veles::").to_string());
                out.insert(first.trim_start_matches("veles.").to_string());
            }
            rest = &after[end + 1..];
        }
    }
    out
}

fn reference_section(doc: &str) -> Option<&str> {
    let start = doc.find(REF_BEGIN)? + REF_BEGIN.len();
    let end = doc[start..].find(REF_END)? + start;
    Some(&doc[start..end])
}

/// The backticked names in the first cell of each table row (header and
/// separator rows skipped).
fn first_cells(section: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in section.lines() {
        let l = line.trim();
        if !l.starts_with('|') || l.starts_with("|-") || l.starts_with("| -") || l.contains("---") {
            continue;
        }
        let cell = l.trim_start_matches('|').split('|').next().unwrap_or("");
        for span in backticked(cell) {
            // The cell's spans as written, not the signature heads the
            // helper adds: the first word of each span is the name.
            if cell.contains(&format!("`{span}`")) {
                let name = span.split_whitespace().next().unwrap_or("");
                out.push(
                    name.trim_start_matches("::veles::")
                        .trim_start_matches("veles.")
                        .to_string(),
                );
            }
        }
    }
    out.retain(|s| !s.is_empty());
    out
}

/// Lines only in `have` (`-`) and only in `want` (`+`), in order, at
/// most 60 of each: enough to see what moved without a diff crate.
fn line_diff(have: &str, want: &str) -> String {
    let h: BTreeSet<&str> = have.lines().collect();
    let w: BTreeSet<&str> = want.lines().collect();
    let mut out = String::new();
    for (i, l) in have.lines().filter(|l| !w.contains(l)).enumerate() {
        if i == 60 {
            out.push_str("- …\n");
            break;
        }
        out.push_str(&format!("- {l}\n"));
    }
    for (i, l) in want.lines().filter(|l| !h.contains(l)).enumerate() {
        if i == 60 {
            out.push_str("+ …\n");
            break;
        }
        out.push_str(&format!("+ {l}\n"));
    }
    if out.is_empty() {
        out.push_str("(the same lines in another order, or whitespace only)\n");
    }
    out
}

// ── reading the surfaces ─────────────────────────────────────────────

/// The JSON object with every object's keys sorted, recursively: one
/// canonical file whatever serde_json's map order is in this build.
fn canonical(v: Value) -> Value {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<(String, Value)> = o.into_iter().collect();
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            let mut m = Map::new();
            for (k, v) in keys {
                m.insert(k, canonical(v));
            }
            Value::Object(m)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(canonical).collect()),
        other => other,
    }
}

fn sorted(it: impl Iterator<Item = &'static str>) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = it.collect();
    v.sort_unstable();
    v.dedup();
    v
}

fn present(list: &[String], name: &str) -> Value {
    if list.iter().any(|s| s == name) {
        Value::String(name.to_string())
    } else {
        Value::Null
    }
}

/// `call_llm` → `callLLM`, `service_call` → `serviceCall`: the JS
/// prelude's naming.
fn js_name(op: &str) -> String {
    let mut out = String::new();
    for (i, part) in op.split('_').enumerate() {
        if i == 0 {
            out.push_str(part);
        } else if part == "llm" {
            out.push_str("LLM");
        } else {
            let mut c = part.chars();
            if let Some(f) = c.next() {
                out.extend(f.to_uppercase());
                out.push_str(c.as_str());
            }
        }
    }
    out
}

fn kebab(op: &str) -> String {
    op.replace('_', "-")
}

/// The field names (or variant names) serde reads for `T`: the list
/// `#[derive(Deserialize)]` hands to `deserialize_struct`, so a renamed
/// or added key shows up here without anyone keeping a copy.
fn struct_fields<T: serde::de::DeserializeOwned>() -> Vec<&'static str> {
    use serde::de::{self, Visitor};

    struct Probe<'a>(&'a RefCell<Vec<&'static str>>);

    impl<'de> de::Deserializer<'de> for Probe<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: Visitor<'de>>(self, _v: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("probe"))
        }

        fn deserialize_struct<V: Visitor<'de>>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _v: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.borrow_mut().extend_from_slice(fields);
            Err(de::Error::custom("probe"))
        }

        fn deserialize_enum<V: Visitor<'de>>(
            self,
            _name: &'static str,
            variants: &'static [&'static str],
            _v: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.borrow_mut().extend_from_slice(variants);
            Err(de::Error::custom("probe"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map identifier ignored_any
        }
    }

    let out = RefCell::new(Vec::new());
    let _ = T::deserialize(Probe(&out));
    out.into_inner()
}

/// `proc ::veles::NAME {ARGS} …` lines of the Tcl stub: `(NAME, ARGS)`.
fn tcl_procs(stub: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in stub.lines() {
        let Some(rest) = line.trim_start().strip_prefix("proc ::veles::") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(args) = braced_word(rest) else {
            continue;
        };
        out.push((name.to_string(), args.to_string()));
    }
    out
}

/// The content of the brace-balanced word `text` starts with.
fn braced_word(text: &str) -> Option<&str> {
    if !text.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[1..i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// A Tcl argument list as a usage line: `{provider ""}` is `?provider?`,
/// `args` is `?arg …?`.
fn tcl_signature(name: &str, args: &str) -> String {
    let mut words = vec![format!("::veles::{name}")];
    let mut rest = args.trim();
    while !rest.is_empty() {
        if rest.starts_with('{') {
            let inner = braced_word(rest).unwrap_or("");
            let arg = inner.split_whitespace().next().unwrap_or("");
            words.push(format!("?{arg}?"));
            rest = rest[inner.len() + 2..].trim_start();
        } else {
            let (w, r) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            words.push(if w == "args" {
                "?arg …?".to_string()
            } else {
                w.to_string()
            });
            rest = r.trim_start();
        }
    }
    words.join(" ")
}

/// The keys of the prelude's `const veles = { … }` object, in order.
fn js_bindings(prelude: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in prelude.lines() {
        if !inside {
            if line.trim_start().starts_with("const veles = {") {
                inside = true;
            }
            continue;
        }
        if line.trim_start().starts_with("};") {
            break;
        }
        // A property sits at the object's own indent (four spaces);
        // deeper lines are its body, `//` lines are comments.
        let Some(body) = line.strip_prefix("    ") else {
            continue;
        };
        if body.starts_with(' ') || body.starts_with("//") {
            continue;
        }
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

/// The words of the parenthesised form that starts at `opener`
/// (`(provide` …`)`), without the opener.
fn paren_list_after(text: &str, opener: &str) -> Vec<String> {
    let Some(start) = text.find(opener) else {
        return Vec::new();
    };
    let body = &text[start + opener.len()..];
    let mut depth = 1usize;
    let mut end = body.len();
    for (i, c) in body.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    body[..end].split_whitespace().map(String::from).collect()
}

/// `pub const NAME: &str = "value";` inside `pub mod method { … }`.
fn protocol_methods(src: &str) -> Vec<(String, String)> {
    let Some(start) = src.find("pub mod method {") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in src[start..].lines().skip(1) {
        let l = line.trim();
        if l == "}" {
            break;
        }
        let Some(rest) = l.strip_prefix("pub const ") else {
            continue;
        };
        let Some((name, rest)) = rest.split_once(':') else {
            continue;
        };
        let Some(value) = rest
            .split_once('"')
            .and_then(|(_, v)| v.split_once('"'))
            .map(|(v, _)| v)
        else {
            continue;
        };
        out.push((name.trim().to_string(), value.to_string()));
    }
    out
}

/// The ABI crate's text with every `//` comment removed (outside string
/// literals), so a doc edit is not an ABI change.
fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        let mut in_str = false;
        let mut prev = '\0';
        let mut cut = line.len();
        for (i, c) in line.char_indices() {
            if c == '"' && prev != '\\' {
                in_str = !in_str;
            }
            if !in_str && c == '/' && prev == '/' {
                cut = i - 1;
                break;
            }
            prev = c;
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

/// A type as text with whitespace collapsed and parameter names dropped
/// (`fn(args_json: *const c_char)` → `fn(*const c_char)`): renaming a
/// parameter is not an ABI change, changing its type is.
fn normalize_type(t: &str) -> String {
    let collapsed = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let collapsed = collapsed.replace("( ", "(").replace(" )", ")");
    let collapsed = collapsed.replace(",)", ")");
    // Drop `ident: ` (a lowercase identifier followed by a single colon
    // and a space); `::` paths never match because their colon is
    // followed by another colon.
    let bytes: Vec<char> = collapsed.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let start_ok = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == '_');
        if start_ok && (bytes[i].is_ascii_lowercase() || bytes[i] == '_') {
            let mut j = i;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == '_') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == ':' && bytes[j + 1] == ' ' {
                i = j + 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Split `text` at top-level commas (not inside (), [] or <>; `->` is
/// not a bracket).
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '-' && chars.get(i + 1) == Some(&'>') {
            cur.push_str("->");
            i += 2;
            continue;
        }
        match c {
            '(' | '[' | '<' => depth += 1,
            ')' | ']' | '>' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut cur));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    if !cur.trim().is_empty() {
        parts.push(cur);
    }
    parts
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The ABI crate's top-level `pub const`s, `pub type`s and `#[repr(C)]`
/// structs, in file order.
fn abi_items(src: &str) -> (Vec<Value>, Vec<Value>, Vec<Value>) {
    let text = strip_comments(src);
    let mut consts = Vec::new();
    let mut types = Vec::new();
    let mut structs = Vec::new();
    let mut rest = text.as_str();
    loop {
        // The next item that starts a line.
        let next = ["\npub const ", "\npub type ", "\npub struct "]
            .iter()
            .filter_map(|p| rest.find(p).map(|i| (i, *p)))
            .min_by_key(|(i, _)| *i);
        let Some((at, kind)) = next else { break };
        let item = &rest[at + kind.len()..];
        match kind {
            "\npub const " | "\npub type " => {
                let Some(end) = item.find(';') else { break };
                let body = &item[..end];
                if kind == "\npub const " {
                    let (name, rest_) = body.split_once(':').unwrap_or((body, ""));
                    let (ty, value) = rest_.split_once('=').unwrap_or((rest_, ""));
                    consts.push(json!({
                        "name": name.trim(),
                        "type": normalize_type(ty),
                        "value": value.trim(),
                    }));
                } else {
                    let (name, ty) = body.split_once('=').unwrap_or((body, ""));
                    types.push(json!({
                        "name": name.trim(),
                        "type": normalize_type(ty),
                    }));
                }
                rest = &item[end + 1..];
            }
            _ => {
                // Only `#[repr(C)]` structs are the C contract.
                let before = &rest[..at];
                let repr_c = before
                    .trim_end()
                    .lines()
                    .last()
                    .map(|l| l.trim() == "#[repr(C)]")
                    .unwrap_or(false);
                let Some(open) = item.find('{') else { break };
                let name = item[..open].trim();
                let mut depth = 0i32;
                let mut close = item.len();
                for (i, c) in item[open..].char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                close = open + i;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                if repr_c {
                    let fields: Vec<Value> = split_top_level(&item[open + 1..close])
                        .into_iter()
                        .filter_map(|f| {
                            let f = f.trim_start_matches("pub ").trim();
                            let (n, t) = f.split_once(':')?;
                            Some(json!({ "name": n.trim(), "type": normalize_type(t) }))
                        })
                        .collect();
                    structs.push(json!({ "name": name, "fields": fields }));
                }
                rest = &item[close.min(item.len())..];
            }
        }
    }
    (consts, types, structs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parsers find what a plugin author reads: if one silently
    /// found nothing, the spec would shrink and the drift checks would
    /// pass on an empty surface.
    #[test]
    fn every_surface_parser_finds_its_known_items() {
        let tcl = tcl_procs(TCL_STUB);
        assert!(tcl
            .iter()
            .any(|(n, a)| n == "call_llm" && a.contains("provider")));
        assert_eq!(
            tcl_signature("call_llm", r#"system user {provider ""} {model ""}"#),
            "::veles::call_llm system user ?provider? ?model?"
        );
        assert_eq!(
            tcl_signature("json_get", "json key args"),
            "::veles::json_get json key ?arg …?"
        );
        let js = js_bindings(JS_PRELUDE);
        for k in ["call", "registerTool", "callLLM", "serviceCall", "jsonGet"] {
            assert!(js.iter().any(|s| s == k), "js lacks {k}: {js:?}");
        }
        assert!(!js.iter().any(|s| s == "args"), "a body line read as a key");
        let racket = paren_list_after(RACKET_API, "(provide");
        assert!(racket.iter().any(|s| s == "register-service"));
        let lisp = paren_list_after(LISP_API, "(:export");
        assert!(lisp.iter().any(|s| s == "#:service-call"));
        let methods = protocol_methods(PROTOCOL_SRC);
        assert!(methods
            .iter()
            .any(|(c, v)| c == "INITIALIZE" && v == "initialize"));
        assert!(methods.iter().any(|(_, v)| v == "service.call"));
        let (consts, types, structs) = abi_items(ABI_SRC);
        assert!(consts
            .iter()
            .any(|c| c["name"] == "VELES_PLUGIN_ABI_VERSION" && c["value"] == "4"));
        assert!(types.iter().any(|t| t["name"] == "VelesHookFn"
            && t["type"] == "unsafe extern \"C\" fn(*const c_char, *mut c_void) -> *mut c_char"));
        let host = structs
            .iter()
            .find(|s| s["name"] == "VelesHostApi")
            .expect("the host table");
        let fields: Vec<&str> = host["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(fields.first(), Some(&"abi_version"));
        assert!(fields.contains(&"register_hook") && fields.contains(&"host_call"));
        assert!(struct_fields::<Manifest>().contains(&"enabled_env"));
        assert!(struct_fields::<veles_core::plugins::PluginManifest>().contains(&"entry"));
    }

    /// Every op a plugin can call has a binding in every script
    /// language: a gap shows up in the spec as `null`, and here.
    #[test]
    fn every_op_has_a_binding_in_every_language() {
        let s = spec();
        let mut gaps = Vec::new();
        for op in s["ops"].as_array().unwrap() {
            for lang in ["tcl", "js", "racket", "lisp"] {
                if op[lang].is_null() {
                    gaps.push(format!("{} in {lang}", op["name"]));
                }
            }
        }
        assert!(gaps.is_empty(), "ops without a binding: {gaps:?}");
    }

    #[test]
    fn a_doc_edit_or_a_parameter_rename_is_not_an_abi_change() {
        assert_eq!(
            normalize_type("unsafe extern \"C\" fn(\n  args_json: *const c_char,\n  ctx: *mut c_void,\n) -> u8"),
            "unsafe extern \"C\" fn(*const c_char, *mut c_void) -> u8"
        );
        assert_eq!(
            strip_comments("pub const A: u32 = 4; // four\n/// doc\n"),
            "pub const A: u32 = 4; \n\n"
        );
        assert_eq!(normalize_type(" std::ffi::c_char "), "std::ffi::c_char");
    }

    #[test]
    fn the_changelog_heading_parses_and_versions_order() {
        let e = changelog_entries("# t\n\n## 0.2 sha256:abcd (x)\n\ntext\n\n## 0.1 sha256:ef01\n");
        assert_eq!(
            e,
            vec![
                ("0.2".to_string(), "abcd".to_string()),
                ("0.1".to_string(), "ef01".to_string())
            ]
        );
        assert!(parse_version("0.10") > parse_version("0.9"));
    }

    /// A scratch checkout holding the spec as the code states it, a
    /// changelog and an author doc.
    fn fixture(changes: &str, doc: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join(SPEC_PATH), spec_text()).unwrap();
        std::fs::write(dir.path().join(CHANGES_PATH), changes).unwrap();
        std::fs::write(dir.path().join(DOC_PATH), doc).unwrap();
        dir
    }

    /// An author doc that is in order: the stamp, every name, and a
    /// reference section whose rows all exist.
    fn good_doc(extra_row: &str, skip: &str) -> String {
        let stamp = stamp_of(&spec_text());
        let names: Vec<String> = doc_names(&spec())
            .into_iter()
            .filter(|n| n != skip)
            .map(|n| format!("`{n}`"))
            .collect();
        format!(
            "# x\n\n{STAMP_PREFIX}{stamp} -->\n\n{}\n\n{REF_BEGIN}\n| name | x |\n|---|---|\n| `call_llm` | x |\n{extra_row}{REF_END}\n",
            names.join(" ")
        )
    }

    /// Each check refuses the drift it exists for, and accepts a
    /// checkout in order. (The real files are checked by
    /// tests/ext_api.rs; this pins the logic, so a mutant that weakens
    /// a check is red even while the real files happen to agree.)
    #[test]
    fn each_check_refuses_its_drift() {
        let stamp = stamp_of(&spec_text());
        let version = stamp.split_whitespace().next().unwrap().to_string();
        let ok_changes = format!("# c\n\n## {stamp} (today)\n\ntext\n");

        let d = fixture(&ok_changes, &good_doc("", ""));
        assert_eq!(check_spec(d.path()), Ok(()));
        assert_eq!(check_changelog(d.path()), Ok(()));
        assert_eq!(check_docs(d.path()), Ok(()));

        // The spec file differs from the code.
        std::fs::write(
            d.path().join(SPEC_PATH),
            spec_text().replace("call_llm", "call_lm"),
        )
        .unwrap();
        assert!(check_spec(d.path()).is_err(), "a changed spec passed");

        // A version repeated down the changelog.
        let d = fixture(
            &format!("{ok_changes}\n## {version} sha256:0000000000000000\n"),
            &good_doc("", ""),
        );
        let e = check_changelog(d.path()).unwrap_err();
        assert!(e.contains("versions must go DOWN"), "{e}");

        // The newest entry names another version than the spec's.
        let d = fixture(
            &ok_changes.replace(&format!("## {version} "), "## 99.0 "),
            &good_doc("", ""),
        );
        assert!(check_changelog(d.path()).is_err(), "a wrong version passed");

        // The newest entry names another hash.
        let d = fixture(
            &ok_changes.replace("sha256:", "sha256:f"),
            &good_doc("", ""),
        );
        assert!(check_changelog(d.path()).is_err(), "a wrong hash passed");

        // The doc's stamp is stale.
        let d = fixture(
            &ok_changes,
            &good_doc("", "").replace("sha256:", "sha256:f"),
        );
        let e = check_docs(d.path()).unwrap_err();
        assert!(e.contains("its stamp says"), "{e}");

        // The doc never names one item of the spec.
        let d = fixture(&ok_changes, &good_doc("", "kanban_task_blocked"));
        let e = check_docs(d.path()).unwrap_err();
        assert!(
            e.contains("never names") && e.contains("`kanban_task_blocked`"),
            "{e}"
        );

        // The doc's reference names what the spec does not have.
        let d = fixture(&ok_changes, &good_doc("| `not_in_the_spec` | x |\n", ""));
        let e = check_docs(d.path()).unwrap_err();
        assert!(e.contains("`not_in_the_spec`"), "{e}");
    }

    #[test]
    fn the_reference_section_reads_first_cells_only() {
        let doc = "x\n<!-- extension-api:reference:begin -->\n| op | js |\n|---|---|\n| `call_llm` | `callLLM` |\n| `::veles::hook name proc` | x |\n<!-- extension-api:reference:end -->\n";
        let s = reference_section(doc).unwrap();
        assert_eq!(
            first_cells(s),
            vec!["call_llm".to_string(), "hook".to_string()]
        );
    }
}
