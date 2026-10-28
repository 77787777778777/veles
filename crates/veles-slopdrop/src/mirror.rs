//! The slopdrop mirror: the state repo, materialized on veles' disk.
//!
//! slopdrop's state is content-addressed (`procs/<sha1>` holds
//! `{args} {body}` under a hash, no name, no file an editor or an LSP
//! can index). A desktop editor over that repo therefore edits a
//! MATERIALIZED VIEW here, under
//! the module's state dir, `<instance>/` (data.rs):
//!
//! ```text
//! .git/                  # veles' own (the Dev pane's project root)
//! _mirror.tsv            # AUTHORITY: <encoded-filename>\t<name>\t<sha1>
//! procs/<encoded>.tcl    # proc <name> {args} { body }
//! vars/<encoded>.tcl     # set <name> <value>  |  array set <name> {…}
//! triggers.tcl           # the live bindings + disable rules, as data
//! ```
//!
//! The mirror is a VIEW. It is not slopdrop's git repo, it pushes to
//! nothing, and a `/api/rollback` on the far side makes it stale —
//! `/slopdrop sync` regenerates it. The one write path is `save`:
//! a proc file saved here is re-parsed and deployed through
//! `POST /api/proc`, the only route that live-updates and commits
//! correctly. A hash drift is refused with a named conflict rather
//! than silently clobbering a proc someone redefined from the channel.
//!
//! Filename encoding is REVERSIBLE (percent-encoding for every byte
//! outside `[A-Za-z0-9._-]`): the live index holds names like
//! `!unknown`, `%niggeria` and a proc whose name is a single space, and
//! a lossy sanitizer would make two procs share one file. Collisions
//! from the length cap get a numeric `~N` suffix, and `_mirror.tsv`
//! records the FILENAME — suffix included. Recording the shared base
//! instead, which is what it did, sent every colliding file to the
//! first one's row: a save on the second drift-checked against the
//! wrong proc's hash, which is the one thing that check exists for.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One slopdrop proc, as /api/procs reports it.
#[derive(Debug, Clone)]
struct Proc {
    name: String,
    args: String,
    body: String,
    hash: String,
}

/// One slopdrop variable, as /api/vars reports it.
#[derive(Debug, Clone)]
struct Var {
    name: String,
    kind: String,
    value: String,
}

/// What `sync` did, for the status line.
#[derive(Debug, Default)]
pub struct SyncReport {
    pub instance: String,
    pub root: PathBuf,
    pub procs: usize,
    pub vars: usize,
    pub dropped: usize, // procs whose blob vanished or would not parse
}

/// What `save` did.
#[derive(Debug)]
pub struct SaveReport {
    pub name: String,
    pub commit: Option<String>,
}

/// What `status` reports — local files only, no HTTP.
#[derive(Debug, Default)]
pub struct StatusReport {
    pub instance: String,
    pub root: PathBuf,
    /// Whether `_mirror.tsv` exists (i.e. a sync has run at least once).
    pub tsv: bool,
    pub procs: usize,
    pub vars: usize,
    /// `_mirror.tsv`'s mtime, as seconds since the epoch — when the
    /// mirror last saw slopdrop.
    pub synced_at: Option<u64>,
}

/// The mirror, as it stands on THIS disk. No HTTP: this is what a
/// `/slopdrop status` answers between syncs, and the same read the
/// desktop's save interception uses.
pub fn status(cfg: &crate::settings::SlopdropSection) -> StatusReport {
    let root = root_for(cfg);
    let tsv = root.join("_mirror.tsv");
    let synced_at = std::fs::metadata(&tsv)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    let count = |sub: &str| -> usize {
        std::fs::read_dir(root.join(sub))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tcl"))
                    .count()
            })
            .unwrap_or(0)
    };
    let procs = count("procs");
    let vars = count("vars");
    StatusReport {
        instance: instance_name_for_url(cfg.url.as_deref()),
        root,
        tsv: tsv.is_file(),
        procs,
        vars,
        synced_at,
    }
}

/// Turn a `/slopdrop save <path>` argument into the file `save` will
/// accept: absolute paths pass through, relative ones resolve inside
/// the mirror's own `procs/` tree — never against the process's cwd,
/// which is whatever the gateway happened to start from.
pub fn resolve_save_path(
    cfg: &crate::settings::SlopdropSection,
    arg: &str,
) -> Result<PathBuf, String> {
    let p = PathBuf::from(arg);
    if p.is_absolute() {
        return Ok(p);
    }
    Ok(root_for(cfg).join("procs").join(p))
}

/// Where one instance's mirror lives.
///
/// `instance` passes through when given (the CLI may name one); empty
/// derives from the LIVE config's bridge URL, so the desktop arriving
/// with no instance still lands on the same tree the gateway synced.
pub fn root(instance: &str) -> PathBuf {
    crate::data::dir().join(instance_name(instance))
}

/// The mirror root for ONE config, derived from ITS bridge URL — the
/// form `sync`/`save` use, so a caller handing a config writes to that
/// config's tree, never to whatever happens to be loaded on disk.
fn root_for(cfg: &crate::settings::SlopdropSection) -> PathBuf {
    crate::data::dir().join(instance_name_for_url(cfg.url.as_deref()))
}

/// The instance name the mirror is filed under: the argument when
/// given, else derived from the live config's bridge URL (host_port),
/// else "default".
fn instance_name(instance: &str) -> String {
    if !instance.trim().is_empty() {
        return instance.trim().to_string();
    }
    instance_name_for_url(crate::settings::settings().url.as_deref())
}

/// (host_port | "default") from a bridge URL — the derivation the name
/// falls back to, separated so `sync(cfg)` can use THE CONFIG IT WAS
/// HANDED instead of loading it a second time.
fn instance_name_for_url(url: Option<&str>) -> String {
    match url {
        Some(u) => {
            let u = u
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            let sanitized: String = u
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || "-._".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if sanitized.is_empty() {
                "default".into()
            } else {
                sanitized
            }
        }
        None => "default".into(),
    }
}

/// Percent-encode every byte of `name` outside `[A-Za-z0-9._-]`, then
/// cap the stem. Reversible, unlike `publish::sanitize_name` — two
/// names must never collapse onto one file. A cap collision (two
/// different very-long names) is disambiguated by a numeric suffix in
/// `_mirror.tsv`, never silently overwritten.
pub fn encode_name(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.len() > 128 {
        // `out` is pure ASCII by construction, so the char boundary is
        // never the problem — a half-written `%XX` escape is. Cutting
        // inside one leaves a stem that no longer decodes, and two names
        // whose escapes differ only past the cut would stop being
        // distinguishable at all.
        let mut cut = 128;
        let b = out.as_bytes();
        if cut >= 1 && b[cut - 1] == b'%' {
            cut -= 1;
        } else if cut >= 2 && b[cut - 2] == b'%' {
            cut -= 2;
        }
        out.truncate(cut);
    }
    // Filesystem-hostile stems fail safely: the empty name, "." and
    // ".." encode to a form that cannot navigate.
    if out.is_empty() {
        out = "_%00empty".into();
    } else if out == "." || out == ".." {
        out = format!("_{out}");
    }
    out
}

/// Assign each proc its mirror filename, in order. Distinct names
/// normally get distinct stems; two that collide on the 128-byte cap
/// are disambiguated with a `~N` suffix — and `~` is itself
/// percent-encoded by `encode_name`, so a suffix can never be confused
/// with part of a name.
///
/// Separated from `sync` so the collision path is reachable without a
/// bridge: it is the branch the module header made a promise about and
/// the tsv did not keep.
fn assign_filenames(names: &[String]) -> Vec<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let enc = encode_name(name);
        let n = counts.entry(enc.clone()).or_insert(0);
        *n += 1;
        out.push(if *n == 1 { enc } else { format!("{enc}~{n}") });
    }
    out
}

/// One row of `_mirror.tsv`. Tab-separated; the encoded name can
/// contain no tab or newline (percent-encoding guarantees it).
fn mirror_line(encoded: &str, real_name: &str, hash: &str) -> String {
    format!("{encoded}\t{real_name}\t{hash}")
}

/// Parse one `_mirror.tsv` row back into (encoded, real name, hash).
fn parse_mirror_line(line: &str) -> Option<(String, String, String)> {
    let mut parts = line.split('\t');
    let encoded = parts.next()?.to_string();
    let name = parts.next()?.to_string();
    let hash = parts.next().unwrap_or("").to_string();
    if encoded.is_empty() {
        return None;
    }
    Some((encoded, name, hash))
}

/// Fetch everything and materialize the mirror. Best-effort on the
/// non-fatal parts: a proc whose blob is gone is counted in `dropped`,
/// never fatal; a `vars` fetch failing does not undo the `procs`
/// fetch that succeeded.
pub async fn sync(cfg: &crate::settings::SlopdropSection) -> Result<SyncReport, String> {
    if !crate::bridge::enabled(cfg) {
        return Err("the slopdrop bridge is not configured ([slopdrop] url)".into());
    }
    let root = root_for(cfg);
    std::fs::create_dir_all(root.join("procs")).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(root.join("vars")).map_err(|e| e.to_string())?;
    ensure_own_git(&root);

    let mut report = SyncReport {
        instance: instance_name_for_url(cfg.url.as_deref()),
        root: root.clone(),
        ..Default::default()
    };

    // ── procs ──
    let procs = fetch_procs(cfg).await?;

    // Assign each proc its encoded filename FIRST, then clear stale
    // files, THEN write — so a file left by a deleted proc can never
    // push a newcomer into a `~1` suffix it does not deserve.
    let names: Vec<String> = procs.iter().map(|p| p.name.clone()).collect();
    let assigned = assign_filenames(&names);
    let wanted: HashSet<&str> = assigned.iter().map(String::as_str).collect();
    remove_stale_files(&root.join("procs"), &wanted);

    // The tsv's first column is the FILENAME, not the shared encoded
    // base. It used to be the base, so two names colliding on the
    // length cap wrote one `foo` row and one `foo~2` file that resolved
    // to the FIRST row's hash — a save on the second would have
    // drift-checked against the wrong proc, which is the one thing the
    // hash check exists to prevent. The header says filename now
    // because that is what it holds.
    let mut tsv = String::from("# filename\tname\thash-at-fetch\n");
    for (p, filename) in procs.iter().zip(assigned.iter()) {
        let path = root.join("procs").join(format!("{filename}.tcl"));
        let rendered = render_proc_file(&p.name, &p.args, &p.body);
        if std::fs::write(&path, rendered).is_err() {
            report.dropped += 1;
            continue;
        }
        tsv.push_str(&mirror_line(filename, &p.name, &p.hash));
        tsv.push('\n');
    }
    report.procs = procs.len().saturating_sub(report.dropped);

    // ── vars ──
    match fetch_vars(cfg).await {
        Ok(vars) => {
            report.vars = vars.len();
            for v in &vars {
                let encoded = encode_name(&v.name);
                let rendered = match v.kind.as_str() {
                    "array" => format!("# {}\narray set {} {}\n", v.name, v.name, v.value),
                    _ => format!("# {}\nset {} {}\n", v.name, v.name, v.value),
                };
                let _ = std::fs::write(root.join("vars").join(format!("{encoded}.tcl")), rendered);
            }
        }
        Err(e) => {
            // The mirror is a view; a vars fetch that fails must not
            // discard procs that fetched fine. Report it, keep going.
            tracing::warn!("slopdrop mirror: vars fetch failed (keeping old): {e}");
        }
    }

    // ── triggers, as data ──
    if let Ok(st) = crate::bridge::triggers_state(cfg).await {
        let mut text = String::from("# slopdrop's live trigger state, materialized READ-ONLY.\n");
        text.push_str("# Edit bindings and disable rules in the channel —\n");
        text.push_str("# `triggers bind` / `triggers disable_for` — then re-sync.\n\n");
        for (event, pattern, proc_name) in &st.bindings {
            text.push_str(&format!("bind {event} {pattern} {proc_name}\n"));
        }
        for (key, procs) in &st.disabled {
            text.push_str(&format!("disable {key}: {}\n", procs.join(" ")));
        }
        let _ = std::fs::write(root.join("triggers.tcl"), text);
    }

    let _ = std::fs::write(root.join("_mirror.tsv"), tsv);
    Ok(report)
}

/// The live proc list from slopdrop, plus name → hash for drift checks.
async fn fetch_procs(cfg: &crate::settings::SlopdropSection) -> Result<Vec<Proc>, String> {
    let json = crate::bridge::get_json(cfg, "/api/procs").await?;
    let mut procs = Vec::new();
    let arr = json
        .as_array()
        .ok_or_else(|| "procs answer is not an array".to_string())?;
    for item in arr {
        let (Some(name), Some(args), Some(body), Some(hash)) = (
            item["name"].as_str(),
            item["args"].as_str(),
            item["body"].as_str(),
            item["hash"].as_str(),
        ) else {
            continue;
        };
        procs.push(Proc {
            name: name.to_string(),
            args: args.to_string(),
            body: body.to_string(),
            hash: hash.to_string(),
        });
    }
    Ok(procs)
}

async fn fetch_vars(cfg: &crate::settings::SlopdropSection) -> Result<Vec<Var>, String> {
    let json = crate::bridge::get_json(cfg, "/api/vars").await?;
    let mut out = Vec::new();
    for item in json.as_array().cloned().unwrap_or_default() {
        let Some(name) = item["name"].as_str() else {
            continue;
        };
        out.push(Var {
            name: name.to_string(),
            kind: item["kind"].as_str().unwrap_or("scalar").to_string(),
            value: item["value"].as_str().unwrap_or("").to_string(),
        });
    }
    Ok(out)
}

/// Render a proc as the file the editor sees: name + args + body under
/// a real `proc` keyword, so the LSP and the eye find the shape they
/// know. The name is always BRACED: the live index holds a proc whose
/// name is a single space, and a bare word cannot render it. Only the
/// NAME (`proc X`) is re-parsed on save; the args and body are re-sent
/// verbatim.
fn render_proc_file(name: &str, args: &str, body: &str) -> String {
    let args = if args.trim().is_empty() {
        ""
    } else {
        args.trim()
    };
    let body = if body.trim().is_empty() { "" } else { body };
    format!("proc {{{name}}} {{{args}}} {{{body}}}\n")
}

/// Deploy an edited proc file. `path` names a file under
/// `root/procs/`; the recorded hash for that name is compared against
/// slopdrop's CURRENT hash first — drift is a named conflict, never a
/// silent clobber (someone redefined the proc from the channel while
/// the editor was open).
pub async fn save(
    cfg: &crate::settings::SlopdropSection,
    path: &Path,
    caller: &str,
) -> Result<SaveReport, String> {
    let root_mirror = root_for(cfg);

    // The file must be inside the mirror's procs/ tree — never a
    // caller-directed path anywhere else.
    let canon_procs = root_mirror
        .join("procs")
        .canonicalize()
        .map_err(|e| format!("mirror procs dir unreadable: {e}"))?;
    let canon_path = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !canon_path.starts_with(&canon_procs) {
        return Err("that path is not a mirror proc file".into());
    }

    let text = std::fs::read_to_string(&canon_path)
        .map_err(|e| format!("read {}: {e}", canon_path.display()))?;
    let (name, args, body) = parse_proc_file(&text)?;

    // The recorded hash at fetch time. Absent means the file was not
    // produced by sync — refuse rather than guess.
    let recorded = tsv_lookup(&root_mirror, &canon_path)?
        .ok_or_else(|| "no _mirror.tsv row for this file (re-sync first)".to_string())?;
    let current = live_hash(cfg, &name).await?;
    if recorded != current {
        return Err(format!(
            "CONFLICT: '{name}' changed on slopdrop since the mirror was synced \
             (mirror has {:.8}…, live has {:.8}…). Re-sync, merge by hand, save again.",
            recorded, current
        ));
    }

    let reply = crate::bridge::post_json(
        cfg,
        "/api/proc",
        serde_json::json!({
            "name": name,
            "args": args,
            "body": body,
            "user": caller,
        }),
    )
    .await?;
    let ok = reply
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !ok {
        return Err(format!(
            "slopdrop refused the deploy: {}",
            reply["message"].as_str().unwrap_or("no reason given")
        ));
    }
    let commit = reply["message"]
        .as_str()
        .and_then(|m| m.split("(commit ").nth(1))
        .map(|s| s.trim_end_matches(')').to_string());
    // The tsv now records the hash WE just produced, so the next save
    // checks drift against the state as of this write, not the stale
    // fetch-time value.
    let new_hash = live_hash(cfg, &name).await?;
    update_tsv_hash(&root_mirror, &canon_path, &new_hash);
    Ok(SaveReport { name, commit })
}

/// (recorded hash, real name) for a tsv row whose encoded filename is
/// this file's stem; None when the file has no row.
fn tsv_lookup(root: &Path, path: &Path) -> Result<Option<String>, String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let tsv = root.join("_mirror.tsv");
    if !tsv.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&tsv).map_err(|e| e.to_string())?;
    for line in text.lines() {
        if let Some((filename, _name, hash)) = parse_mirror_line(line) {
            // EXACT, because the first column is the filename now. The
            // old base comparison sent `foo~2.tcl` to `foo`'s row.
            if stem == filename {
                return Ok(Some(hash));
            }
        }
    }
    Ok(None)
}

fn update_tsv_hash(root: &Path, path: &Path, new_hash: &str) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let tsv = root.join("_mirror.tsv");
    let Ok(text) = std::fs::read_to_string(&tsv) else {
        return;
    };
    let mut out = String::new();
    for line in text.lines() {
        if let Some((filename, name, _h)) = parse_mirror_line(line) {
            if stem == filename {
                out.push_str(&mirror_line(&filename, &name, new_hash));
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    let _ = std::fs::write(&tsv, out);
}

/// slopdrop's current hash for one proc name ("" when deleted).
async fn live_hash(cfg: &crate::settings::SlopdropSection, name: &str) -> Result<String, String> {
    let json = crate::bridge::get_json(cfg, "/api/procs?names=1").await?;
    for item in json.as_array().cloned().unwrap_or_default() {
        if item["name"].as_str() == Some(name) {
            return Ok(item["hash"].as_str().unwrap_or_default().to_string());
        }
    }
    Ok(String::new())
}

/// Parse a rendered proc file: `proc <name> {args} { body }` →
/// (name, args, body). The name may be a bare word OR braced (the
/// renderer braces it: the live index has a proc whose name is a
/// single space). The args/body braces may nest; a body may span many
/// lines. Deliberately small: this parses what `sync` writes, not
/// arbitrary Tcl.
pub fn parse_proc_file(text: &str) -> Result<(String, String, String), String> {
    let trimmed = text.trim();
    let Some(rest) = trimmed.strip_prefix("proc") else {
        return Err("not a proc file (missing 'proc')".into());
    };
    let mut chars = rest.trim_start().chars().peekable();

    // name: a bare word, or a braced string (the renderer's form).
    let mut name = String::new();
    if chars.peek() == Some(&'{') {
        chars.next();
        let mut depth = 1usize;
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    depth += 1;
                    name.push(c);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    name.push(c);
                }
                '\\' => {
                    if let Some(n) = chars.next() {
                        name.push(n);
                    }
                }
                _ => name.push(c),
            }
        }
    } else {
        while let Some(&c) = chars.peek() {
            if c.is_whitespace() || c == '{' {
                break;
            }
            name.push(c);
            chars.next();
        }
    }
    if name.is_empty() {
        return Err("proc file has no name".into());
    }

    fn take_braced(chars: &mut std::iter::Peekable<std::str::Chars>) -> Result<String, String> {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.next() != Some('{') {
            return Err("expected '{'".into());
        }
        let mut depth = 1usize;
        let mut out = String::new();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    depth += 1;
                    out.push(c);
                }
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(out);
                    }
                    out.push(c);
                }
                '\\' => {
                    out.push(c);
                    if let Some(n) = chars.next() {
                        out.push(n);
                    }
                }
                _ => out.push(c),
            }
        }
        Err("unbalanced braces".into())
    }

    let args = take_braced(&mut chars)?;
    // The body may be absent in a hand-edited one-liner; `sync` always
    // writes one. Absent reads back as the empty body.
    let body = take_braced(&mut chars).unwrap_or_default();
    Ok((name, args, body))
}

/// Make `.git` exist so the Dev pane (and lsp_proxy) treats the mirror
/// as a project root. veles' OWN repo — never slopdrop's, never pushed.
fn ensure_own_git(root: &Path) {
    if root.join(".git").exists() {
        return;
    }
    let _ = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(root)
        .output();
}

/// Remove mirror files that no live proc maps to: a deleted proc must
/// not stay behind as a shadow the editor still shows.
fn remove_stale_files(dir: &Path, wanted: &HashSet<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let Some(stem) = e
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .map(String::from)
        else {
            continue;
        };
        // `wanted` holds FILENAMES, suffix included, so this is exact:
        // a `x~2.tcl` left behind by a collision that no longer happens
        // is stale and goes, which the base comparison used to keep.
        if wanted.contains(stem.as_str()) {
            continue;
        }
        // Only *.tcl files are mirror artifacts; leave anything else.
        if e.path().extension().and_then(|x| x.to_str()) == Some("tcl") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_filename_encoding_never_navigates_and_never_collides() {
        // The spec's own list: the live index's bad names plus the
        // filesystem traps.
        for name in [
            "",
            " ",
            ".",
            "..",
            "a/b",
            "CON",
            "!unknown",
            "%niggeria",
            "%weed",
            "%pagoda",
            "::namespace::proc",
            "pro*?name",
            "\t\n",
            "straße",
        ] {
            let enc = encode_name(name);
            assert!(
                !enc.is_empty() && enc != "." && enc != "..",
                "encoding of {name:?} must not be a path traversal: {enc:?}"
            );
            assert!(
                !enc.contains(['/', '\\', '\t', '\n']),
                "encoding of {name:?} must be one path segment: {enc:?}"
            );
        }
        // Reversibility is proven by the tsv; the filename only has to
        // be injective within one sync. Distinct names → distinct stems.
        let mut seen = HashSet::new();
        for name in ["a", "A", "a b", "a%b", "a.b", "b", "a\tb"] {
            assert!(seen.insert(encode_name(name)), "{name} collided");
        }
        // The 300-char name truncates without panicking and stays one
        // segment, on a char boundary.
        let long = "x".repeat(300);
        let enc = encode_name(&long);
        assert!(enc.len() <= 128, "capped: {}", enc.len());
        assert!(enc.is_char_boundary(enc.len()));
    }

    /// B3 — the module header promised that a cap collision "gets a
    /// numeric suffix recorded in `_mirror.tsv`". The suffix reached the
    /// FILE; the tsv recorded the shared encoded BASE. So both files
    /// resolved to the FIRST row, and a save on the second would have
    /// compared its hash against the wrong proc — defeating the one
    /// check that stops a silent clobber of something someone redefined
    /// from the channel.
    #[test]
    fn a_cap_collision_gets_its_own_file_and_its_own_tsv_row() {
        // Two distinct names that the 128-byte cap folds together.
        let base = "x".repeat(140);
        let a = format!("{base}-alpha");
        let b = format!("{base}-beta");
        assert_eq!(
            encode_name(&a),
            encode_name(&b),
            "the cap is what collides them; the test needs that to hold"
        );

        let files = assign_filenames(&[a.clone(), b.clone()]);
        assert_eq!(files.len(), 2);
        assert_ne!(files[0], files[1], "two procs, two files");
        assert!(files[1].ends_with("~2"), "{}", files[1]);

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("procs")).expect("procs");
        let mut tsv = String::new();
        for (f, (name, hash)) in files
            .iter()
            .zip([(&a, "hash-alpha"), (&b, "hash-beta")].iter())
        {
            tsv.push_str(&mirror_line(f, name, hash));
            tsv.push('\n');
        }
        std::fs::write(root.join("_mirror.tsv"), tsv).expect("tsv");

        let pa = root.join("procs").join(format!("{}.tcl", files[0]));
        let pb = root.join("procs").join(format!("{}.tcl", files[1]));
        assert_eq!(
            tsv_lookup(root, &pa).unwrap().as_deref(),
            Some("hash-alpha")
        );
        assert_eq!(
            tsv_lookup(root, &pb).unwrap().as_deref(),
            Some("hash-beta"),
            "the suffixed file must resolve to its OWN row, not the first"
        );

        // A save on one rewrites only its own row.
        update_tsv_hash(root, &pb, "hash-beta-2");
        assert_eq!(
            tsv_lookup(root, &pa).unwrap().as_deref(),
            Some("hash-alpha"),
            "updating one row must not touch the other's hash"
        );
        assert_eq!(
            tsv_lookup(root, &pb).unwrap().as_deref(),
            Some("hash-beta-2")
        );
    }

    /// The cap must not cut inside a `%XX` escape: a stem ending in a
    /// dangling `%` or `%4` no longer decodes, and two names differing
    /// only past the cut stop being distinguishable at all.
    #[test]
    fn the_length_cap_never_splits_an_escape() {
        // Pad so the 128-byte boundary lands inside an escape.
        for pad in 125..=128 {
            let name = format!("{}{}", "x".repeat(pad), "::tail");
            let enc = encode_name(&name);
            assert!(enc.len() <= 128, "capped: {}", enc.len());
            // Every `%` in the result is followed by two hex digits.
            let b = enc.as_bytes();
            let mut i = 0;
            while i < b.len() {
                if b[i] == b'%' {
                    assert!(
                        i + 2 < b.len(),
                        "dangling escape at {i} in {enc:?} (pad {pad})"
                    );
                    assert!(b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit());
                    i += 3;
                } else {
                    i += 1;
                }
            }
        }
    }

    #[test]
    fn parse_proc_file_returns_the_three_parts() {
        let (name, args, body) =
            parse_proc_file("proc timtom::welcome {nick chan} {\n  puts \"hi $nick\"\n}").unwrap();
        assert_eq!(name, "timtom::welcome");
        assert_eq!(args, "nick chan");
        assert_eq!(body, "\n  puts \"hi $nick\"\n");

        // Nested braces in the body survive.
        let (_, _, body2) = parse_proc_file("proc nested {} { if {1} { puts {ok} }\n }").unwrap();
        assert_eq!(body2, " if {1} { puts {ok} }\n ");

        // The degenerate empty shapes sync itself writes.
        let (n, a, b) = parse_proc_file("proc empty {} {}\n").unwrap();
        assert_eq!((n.as_str(), a.as_str(), b.as_str()), ("empty", "", ""));

        assert!(parse_proc_file("set x 1").is_err());
        assert!(parse_proc_file("proc {} {}").is_err(), "no name");
    }

    #[test]
    fn mirror_lines_parse_back_verbatim() {
        let line = mirror_line("timtom%3A%3Awelcome", "timtom::welcome", "abc123");
        let (enc, name, hash) = parse_mirror_line(&line).unwrap();
        assert_eq!(enc, "timtom%3A%3Awelcome");
        assert_eq!(name, "timtom::welcome");
        assert_eq!(hash, "abc123");
        assert!(parse_mirror_line("no tabs here").is_none());
        assert!(parse_mirror_line("").is_none());
    }

    #[test]
    fn the_render_parse_round_trip_is_verbatim() {
        let rendered = render_proc_file("weird proc", "a b", "if {1} { puts x }\n");
        assert_eq!(rendered, "proc {weird proc} {a b} {if {1} { puts x }\n}\n");
        let (name, args, body) = parse_proc_file(&rendered).unwrap();
        assert_eq!(name, "weird proc");
        assert_eq!(args, "a b");
        assert_eq!(body, "if {1} { puts x }\n");
    }

    #[test]
    fn the_instance_name_derives_from_the_url() {
        assert_eq!(
            instance_name_for_url(Some("http://10.0.30.1:8080/")),
            "10.0.30.1_8080"
        );
        assert_eq!(
            instance_name_for_url(Some("https://vps.example:8443")),
            "vps.example_8443"
        );
        assert_eq!(instance_name_for_url(None), "default");
        assert_eq!(instance_name("vps"), "vps");
        // The empty-name passthrough loads the LIVE config, which a
        // unit test must not depend on — that branch is covered by the
        // pure derivation above.
    }

    // ── the wire, against a real socket (slopdrop.rs's listener
    // pattern) ──────────────────────────────────────────────────────

    use crate::settings::SlopdropSection;

    fn cfg_for(url: &str) -> SlopdropSection {
        SlopdropSection {
            enabled: true,
            url: Some(url.to_string()),
            token_env: None,
            admin_token_env: None,
            timeout_ms: Some(5_000),
            output_classifier: Vec::new(),
            output_classifier_tox: None,
            output_classifier_id: None,
            bsky_read_url: None,
        }
    }

    /// The script a stub connection serves, keyed on the request line.
    /// `procs` answers GET /api/procs, `names1` the ?names=1 variant,
    /// `vars` GET /api/vars, `proc_response` POST /api/proc.
    fn serve_one(
        line: &str,
        procs: &str,
        names1: &str,
        vars: &str,
        proc_response: &str,
    ) -> Option<String> {
        if line.starts_with("GET /api/procs") {
            if line.contains("names=1") {
                return Some(names1.to_string());
            }
            return Some(procs.to_string());
        }
        if line.starts_with("GET /api/vars") {
            return Some(vars.to_string());
        }
        if line.starts_with("GET /api/triggers") {
            return Some(r#"{"bindings":[],"disabled":[]}"#.to_string());
        }
        if line.starts_with("POST /api/proc") {
            return Some(proc_response.to_string());
        }
        None
    }

    /// A small HTTP stub on an ephemeral port answering the given
    /// script, one connection at a time for up to 12 requests — enough
    /// for `sync` (3) plus one `save` (a few more). Returns the port
    /// and a join handle whose result is the list of request lines
    /// actually seen.
    fn spawn_stub<F>(script: F) -> (u16, tokio::task::JoinHandle<Vec<String>>)
    where
        F: Fn(String) -> Option<String> + Send + 'static,
    {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = veles_core::rt::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let mut seen = Vec::new();
            for _ in 0..12 {
                let conn =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await;
                let Ok(Ok((mut sock, _))) = conn else {
                    break;
                };
                let mut buf = vec![0u8; 16384];
                let _ = sock.read(&mut buf).await;
                let req = String::from_utf8_lossy(&buf).to_string();
                let first = req.lines().next().unwrap_or_default().to_string();
                // The WHOLE request, not the line: the deploy assertions
                // check the body (`user`, the edited proc text).
                seen.push(req.clone());
                let body = script(first).unwrap_or_else(|| r#"{}"#.to_string());
                let ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n";
                let _ = sock.write_all(ok.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
            }
            seen
        });
        (port, handle)
    }

    /// The whole mirror cycle, conflict half included: `sync`
    /// materializes procs (hostile names among them) and `save`
    /// refuses a drifted hash BY NAME, without ever reaching the
    /// deploy endpoint.
    #[tokio::test]
    async fn sync_materializes_procs_and_save_drift_is_refused_named() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let (port, srv) = spawn_stub(|line| {
            serve_one(
                &line,
                r#"[
                    {"name":"nitterize","args":"u","body":"puts x","hash":"deadbeef0000"},
                    {"name":" ","args":"","body":"puts sp","hash":"s p a c e"},
                    {"name":"!unknown","args":"a","body":"?","hash":"bang"},
                    {"name":"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-alpha","args":"","body":"a","hash":"hashA"},
                    {"name":"yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy-beta","args":"","body":"b","hash":"hashB"}
                ]"#,
                r#"[{"name":"nitterize","hash":"LIVEhash-elsewhere"}]"#,
                r#"[{"name":"n","kind":"scalar","value":"1"}]"#,
                r#"{"success":false,"message":"must not be called"}"#,
            )
        });
        let cfg = cfg_for(&format!("http://127.0.0.1:{port}"));

        let r = sync(&cfg).await.expect("sync must succeed");
        assert_eq!(r.procs, 5, "three odd names plus the colliding pair");
        assert_eq!(r.vars, 1);
        assert_eq!(r.dropped, 0);
        let root = r.root.clone();
        assert!(root.join("_mirror.tsv").is_file());
        let tsv = std::fs::read_to_string(root.join("_mirror.tsv")).unwrap();
        assert!(tsv.contains("\t!unknown\t"), "the index names the proc");

        // Two names that the 128-byte cap folds together get two FILES
        // and two tsv ROWS — with the `~N` suffix in the first column,
        // because that column is the filename. Recording the shared
        // base sent both files to the first row's hash, and a save on
        // the second would then have drift-checked the wrong proc.
        let base = encode_name(&format!("{}-alpha", "y".repeat(140)));
        assert!(
            root.join("procs").join(format!("{base}.tcl")).is_file(),
            "the first of the colliding pair"
        );
        assert!(
            root.join("procs").join(format!("{base}~2.tcl")).is_file(),
            "the second gets its own file"
        );
        assert!(
            tsv.lines().any(|l| l.starts_with(&format!("{base}~2\t"))),
            "and its own tsv row, keyed by the FILENAME: {tsv}"
        );
        assert!(
            tsv.contains("\thashA\n") && tsv.contains("\thashB\n"),
            "each colliding row keeps its own hash: {tsv}"
        );
        let rendered = std::fs::read_to_string(root.join("procs").join("nitterize.tcl")).unwrap();
        assert!(
            rendered.starts_with("proc {nitterize}") || rendered.starts_with("proc nitterize"),
            "rendered: {rendered}"
        );

        // SAVE with hash drift: names=1 answers LIVEhash-elsewhere,
        // the mirror recorded deadbeef0000 — named refusal, no deploy.
        let path = root.join("procs").join("nitterize.tcl");
        let err = save(&cfg, &path, "irc:whomever")
            .await
            .expect_err("drift must refuse");
        assert!(err.contains("CONFLICT"), "named: {err}");
        assert!(err.contains("nitterize"), "names the proc: {err}");

        let seen = srv.await.unwrap();
        assert!(
            seen.iter().all(|l| !l.starts_with("POST /api/proc")),
            "the conflict case must never reach the deploy endpoint: {seen:?}"
        );
    }

    /// The happy write: recorded hash matches live, /api/proc deploys
    /// with the caller as `user`, and the success message carries the
    /// commit.
    #[tokio::test]
    async fn save_deploys_through_the_interpreter_when_hashes_match() {
        let (_home, _env) = veles_core::testenv::scratch_home();
        let (port, srv) = spawn_stub(|line| {
            serve_one(
                &line,
                r#"[{"name":"nitterize","args":"u","body":"old","hash":"deadbeef0000"}]"#,
                r#"[{"name":"nitterize","hash":"deadbeef0000"}]"#,
                r#"[]"#,
                r#"{"success":true,"message":"+proc: nitterize (commit 0123abc)"}"#,
            )
        });
        let cfg = cfg_for(&format!("http://127.0.0.1:{port}"));
        let r = sync(&cfg).await.unwrap();
        let path = r.root.join("procs").join("nitterize.tcl");
        std::fs::write(&path, "proc nitterize {u} {puts NEW}\n").unwrap();
        let saved = save(&cfg, &path, "irc:whomever").await.expect("deploy");
        assert_eq!(saved.name, "nitterize");
        assert_eq!(saved.commit.as_deref(), Some("0123abc"));

        let seen = srv.await.unwrap();
        let deploy = seen
            .iter()
            .find(|l| l.starts_with("POST /api/proc"))
            .expect("save must reach the deploy endpoint");
        assert!(
            deploy.contains("\"user\":\"irc:whomever\""),
            "the caller rides as user, not the bridge: {deploy}"
        );
        assert!(
            deploy.contains("puts NEW"),
            "the edited body rides verbatim: {deploy}"
        );
    }
}
